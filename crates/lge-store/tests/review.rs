//! Inputs the first tests did not cover: hostile text, failed transactions,
//! the re-index path, extreme limits and id lists, and reopening.

mod common;

use common::*;
use lge_core::{
    file_id, kind, lang, method, service_id, unresolved_id, Direction, EdgeRec, EdgeType, HandoffNote, NodeLabel,
    NodeRec, ParsedFile, Resolved, UsageRecord,
};
use lge_store::{GraphStore, Store};

/// The single number a `RETURN count(...)` query prints.
fn count(store: &Store, cypher: &str) -> i64 {
    store
        .raw(cypher)
        .unwrap()
        .lines()
        .nth(1)
        .unwrap_or_else(|| panic!("no row from {cypher}"))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("{cypher}: {e}"))
}

/// Text that breaks a query assembled by string concatenation.
const HOSTILE: &[&str] = &[
    "plain",
    "it's",
    "say \"hi\"",
    "back\\slash",
    "ends with backslash\\",
    "line\nbreak",
    "carriage\r\nreturn",
    "tab\there",
    "{braces} and ${dollar} and $param",
    "'}) DETACH DELETE n; //",
    "`backtick` and -- comment /* block */",
    "unicode \u{00e9}\u{4e2d}\u{6587} \u{1f980}",
    "",
    "   ",
    "NULL",
    "percent % and _ underscore",
];

fn hostile_file(i: usize, text: &str) -> ParsedFile {
    let path = format!("svc/{i}/{text}.py");
    let f = file(&path, "svc", lang::PYTHON);
    let mut s = sym(&f, &format!("svc.m{i}.{text}"), kind::FUNCTION, 1, 2);
    s.name = text.to_string();
    s.signature = format!("def f({text})");
    s.doc = text.to_string();
    s.body = format!("body {text} body");
    let mut c = call(&s, text, text, 3);
    c.id = format!("call-{i}");
    let mut im = import(&f, text, text, text);
    im.id = format!("import-{i}");
    let mut fa = fact_site(&s, text, text, text, 4, true);
    fa.id = format!("fact-{i}");
    ParsedFile {
        file: Some(f),
        symbols: vec![s],
        calls: vec![c],
        imports: vec![im],
        facts: vec![fa],
    }
}

#[test]
fn hostile_text_is_stored_and_read_back_exactly() {
    let (_dir, store) = open();
    let files: Vec<ParsedFile> = HOSTILE.iter().enumerate().map(|(i, t)| hostile_file(i, t)).collect();
    for p in &files {
        store.insert_parsed(p, "commit 'x' \"y\" \\ $z", NOW).unwrap();
    }

    let input = store.load_resolve_input().unwrap();
    assert_eq!(input.files.len(), HOSTILE.len());
    for p in &files {
        let f = p.file.as_ref().unwrap();
        let s = &p.symbols[0];
        let info = store
            .file_info(&f.path)
            .unwrap()
            .unwrap_or_else(|| panic!("{:?}", f.path));
        assert_eq!((info.id.as_str(), info.path.as_str()), (f.id.as_str(), f.path.as_str()));
        assert_eq!(info.indexed_commit, "commit 'x' \"y\" \\ $z");
        assert_eq!(
            store.file_hashes().unwrap()[&f.path],
            (f.id.clone(), f.content_hash.clone())
        );

        assert_eq!(
            store.symbols_in_file(&f.path).unwrap(),
            vec![node_of(s)],
            "{:?}",
            f.path
        );
        assert_eq!(store.nodes(std::slice::from_ref(&s.id)).unwrap(), vec![node_of(s)]);
        assert_eq!(store.symbol_body(&s.id).unwrap(), Some(s.body.clone()));
        let by_qn = store.find(&s.qualified_name).unwrap();
        assert_eq!(by_qn.len(), 1, "{:?}", s.qualified_name);
        assert_eq!(by_qn[0].id, s.id);

        let mut expected = s.clone();
        expected.body = String::new();
        assert!(input.symbols.contains(&expected), "{:?}", s.name);
        assert!(input.calls.contains(&p.calls[0]), "{:?}", s.name);
        assert!(input.imports.contains(&p.imports[0]), "{:?}", s.name);
        assert!(input.facts.contains(&p.facts[0]), "{:?}", s.name);
    }

    // Edges whose evidence and endpoints carry the same text, by statements.
    let ids: Vec<String> = files.iter().map(|p| p.symbols[0].id.clone()).collect();
    let mut resolved = Resolved::default();
    for (i, text) in HOSTILE.iter().enumerate() {
        let placeholder = NodeRec::bare(&unresolved_id(text), NodeLabel::Symbol, text, text, kind::UNRESOLVED);
        let endpoint = NodeRec {
            service: text.to_string(),
            file_path: text.to_string(),
            ..NodeRec::bare(&format!("ep:{text}"), NodeLabel::ApiEndpoint, text, text, text)
        };
        let table = NodeRec::bare(&format!("tb:{text}"), NodeLabel::DbTable, text, text, "");
        let queue = NodeRec::bare(&format!("q:{text}"), NodeLabel::Queue, text, text, text);
        resolved.edges.extend([
            edge(
                &ids[i],
                &placeholder.id,
                EdgeType::Calls,
                0.0,
                method::UNRESOLVED,
                text,
                1,
                2,
            ),
            edge(
                &ids[i],
                &endpoint.id,
                EdgeType::ExposesApi,
                0.9,
                method::STATIC,
                text,
                1,
                2,
            ),
            edge(
                &ids[i],
                &table.id,
                EdgeType::ReadsFrom,
                0.5,
                method::INFERRED,
                text,
                1,
                2,
            ),
            edge(&ids[i], &queue.id, EdgeType::Publishes, 0.9, method::STATIC, text, 1, 2),
        ]);
        resolved.nodes.extend([placeholder, endpoint, table, queue]);
    }
    store.replace_edges(&resolved, "commit 'x'", NOW).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    assert_eq!(
        sorted(store.edges(&ids, &[], Direction::Out).unwrap()),
        sorted(resolved.edges.clone())
    );
    let targets: Vec<String> = resolved.edges.iter().map(|e| e.to.clone()).collect();
    assert_eq!(
        sorted(store.edges(&targets, &[], Direction::In).unwrap()),
        sorted(resolved.edges.clone())
    );
    let node_ids: Vec<String> = resolved.nodes.iter().map(|n| n.id.clone()).collect();
    let mut read = store.nodes(&node_ids).unwrap();
    let mut expected = resolved.nodes.clone();
    for n in &mut expected {
        // DbTable and Queue have one name column.
        if matches!(n.label, NodeLabel::DbTable | NodeLabel::Queue) {
            n.qualified_name = n.name.clone();
        }
    }
    read.sort_by(|a, b| (a.label, &a.id).cmp(&(b.label, &b.id)));
    expected.sort_by(|a, b| (a.label, &a.id).cmp(&(b.label, &b.id)));
    assert_eq!(read, expected);

    // Nothing was executed: every table still has its rows.
    assert_eq!(count(&store, "MATCH (f:File) RETURN count(f);"), HOSTILE.len() as i64);
    assert_eq!(stat(&store, "symbols"), HOSTILE.len() as i64);

    // Deleting by a hostile path removes exactly that file.
    for (i, p) in files.iter().enumerate() {
        store.delete_file(&p.file.as_ref().unwrap().path).unwrap();
        assert_eq!(stat(&store, "files"), (HOSTILE.len() - i - 1) as i64);
    }
    assert_eq!(stat(&store, "symbols"), 0);
    assert_eq!(stat(&store, "call_sites"), 0);
    assert_eq!(stat(&store, "fact_sites"), 0);
    assert_eq!(count(&store, "MATCH (i:ImportSite) RETURN count(i);"), 0);
    assert_eq!(stat(&store, "services"), 0);
}

#[test]
fn hostile_text_survives_the_bulk_edge_load() {
    let (_dir, store) = open();
    let files: Vec<ParsedFile> = HOSTILE.iter().enumerate().map(|(i, t)| hostile_file(i, t)).collect();
    store.insert_parsed_bulk(&files, COMMIT, NOW).unwrap();
    let ids: Vec<String> = files.iter().map(|p| p.symbols[0].id.clone()).collect();

    let mut resolved = Resolved::default();
    // Endpoint ids a CSV reader could split or unquote.
    for (i, text) in HOSTILE.iter().enumerate() {
        let table = NodeRec::bare(&format!("tb:{text}"), NodeLabel::DbTable, text, text, "");
        let placeholder = NodeRec::bare(&unresolved_id(text), NodeLabel::Symbol, text, text, kind::UNRESOLVED);
        resolved.edges.extend([
            edge(&ids[i], &table.id, EdgeType::WritesTo, 0.9, method::STATIC, text, 1, 2),
            edge(
                &ids[i],
                &placeholder.id,
                EdgeType::Inherits,
                0.0,
                method::UNRESOLVED,
                text,
                1,
                2,
            ),
        ]);
        resolved.nodes.extend([table, placeholder]);
    }
    // Confidences at the edges of what a decimal text can carry.
    let confidences = [
        0.0,
        -0.0,
        f64::MIN_POSITIVE,
        5e-324,
        f64::MAX,
        f64::MIN,
        1e300,
        1e-300,
        0.1 + 0.2,
        1.0 / 3.0,
        0.95,
        0.4,
    ];
    for (i, c) in confidences.iter().enumerate() {
        resolved.edges.push(edge(
            &ids[i],
            &ids[i + 1],
            EdgeType::References,
            *c,
            method::INFERRED,
            "NULL",
            i as i64,
            0,
        ));
    }
    // Padding, so the bulk load is what runs.
    for round in 0..200 {
        for i in 0..ids.len() {
            let to = &ids[(i + 1 + round % (ids.len() - 1)) % ids.len()];
            resolved.edges.push(edge(
                &ids[i],
                to,
                EdgeType::Calls,
                0.5,
                method::INFERRED,
                "pad.py",
                round as i64,
                1,
            ));
        }
    }
    assert!(resolved.edges.len() >= 2000);
    let hostile_commit = "c'om\"m,it\\";
    store.replace_edges(&resolved, hostile_commit, NOW).unwrap();
    assert_eq!(store.last_bulk_load_error(), None);
    assert_eq!(store.last_skipped_edges(), 0);

    let read = store.edges(&ids, &[], Direction::Out).unwrap();
    assert_eq!(read.len(), resolved.edges.len());
    let key = |e: &EdgeRec| {
        (
            e.edge_type,
            e.from.clone(),
            e.to.clone(),
            e.start_line,
            e.confidence.to_bits(),
            e.method.clone(),
            e.evidence_file.clone(),
            e.end_line,
        )
    };
    let mut got: Vec<_> = read.iter().map(key).collect();
    let mut want: Vec<_> = resolved.edges.iter().map(key).collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
    for t in ["WRITES_TO", "INHERITS", "REFERENCES", "CALLS"] {
        assert_eq!(
            count(&store, &format!("MATCH ()-[r:{t}]->() WHERE r.commit <> 'c\\'om\"m,it\\\\' OR r.commit IS NULL OR r.evidence_file IS NULL RETURN count(r);")),
            0,
            "{t}"
        );
    }
}

#[test]
fn a_failed_insert_changes_nothing_and_leaves_the_store_usable() {
    let (_dir, store, fx, resolved) = indexed();
    let all_ids: Vec<String> = resolved
        .edges
        .iter()
        .flat_map(|e| [e.from.clone(), e.to.clone()])
        .collect();
    let snapshot = |s: &Store| {
        (
            s.stats().unwrap(),
            s.edges(&all_ids, &EdgeType::ALL, Direction::Out).unwrap(),
            NodeLabel::ALL.map(|l| s.nodes_by_label(l, 1000).unwrap()),
            s.file_hashes().unwrap(),
            {
                let i = s.load_resolve_input().unwrap();
                (i.files, i.symbols, i.calls, i.imports, i.facts)
            },
            s.search("create order", 10).unwrap(),
        )
    };
    let before = snapshot(&store);

    // A new file whose second symbol has the id of a symbol of another file.
    let f = file("billing/invoice.py", "billing", lang::PYTHON);
    let fresh = sym(&f, "billing.invoice.total", kind::FUNCTION, 1, 5);
    let mut clash = sym(&f, "billing.invoice.other", kind::FUNCTION, 6, 9);
    clash.id = fx.id("orders_api.repo.save_order");
    let bad = ParsedFile {
        calls: vec![call(&fresh, "x", "", 2)],
        imports: vec![import(&f, "m", "n", "n")],
        facts: vec![fact_site(&fresh, "db_read", "t", "", 3, true)],
        file: Some(f),
        symbols: vec![fresh, clash],
    };
    assert!(store.insert_parsed(&bad, COMMIT, NOW).is_err());
    assert_eq!(snapshot(&store), before);
    assert!(store.file_info("billing/invoice.py").unwrap().is_none());
    assert!(store.nodes(&[service_id("billing")]).unwrap().is_empty());
    assert!(
        store.search("total", 10).unwrap().is_empty(),
        "the rolled back symbol is searchable"
    );

    // Replacing an indexed file fails half way: the old version stays, with its edges.
    let mut changed = fx.get(ROUTES_PY).clone();
    changed.symbols[1].doc = "changed".to_string();
    let mut clash = changed.symbols[2].clone();
    clash.id = fx.id("orders_api.repo.save_order");
    changed.symbols.push(clash);
    assert!(store.insert_parsed(&changed, "other", NOW + 50).is_err());
    assert_eq!(snapshot(&store), before);
    assert_eq!(store.file_info(ROUTES_PY).unwrap().unwrap().indexed_commit, COMMIT);

    // The same through the bulk path, with a good file in the same batch.
    let good = parsed(file("billing/ok.py", "billing", lang::PYTHON), vec![]);
    assert!(store
        .insert_parsed_bulk(&[good.clone(), bad.clone()], COMMIT, NOW)
        .is_err());
    assert_eq!(snapshot(&store), before);

    // A duplicate staged-site id from another file fails the same way.
    let f = file("billing/calls.py", "billing", lang::PYTHON);
    let s = sym(&f, "billing.calls.f", kind::FUNCTION, 1, 5);
    let mut dup = call(&s, "x", "", 2);
    dup.id = fx.get(ROUTES_PY).calls[0].id.clone();
    let bad_call = ParsedFile {
        calls: vec![dup],
        file: Some(f),
        symbols: vec![s],
        ..ParsedFile::default()
    };
    assert!(store.insert_parsed(&bad_call, COMMIT, NOW).is_err());
    assert_eq!(snapshot(&store), before);

    // Still writable: the next insert, delete and resolve all work.
    store.insert_parsed(&good, COMMIT, NOW).unwrap();
    assert_eq!(stat(&store, "files"), fx.files.len() as i64 + 1);
    store.delete_file("billing/ok.py").unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    assert_eq!(snapshot(&store), before);
}

#[test]
fn a_failed_replace_edges_keeps_the_previous_graph() {
    let (_dir, store, fx, resolved) = indexed();
    let all_ids: Vec<String> = resolved
        .edges
        .iter()
        .flat_map(|e| [e.from.clone(), e.to.clone()])
        .collect();
    let snapshot = |s: &Store| {
        (
            s.stats().unwrap(),
            s.edges(&all_ids, &EdgeType::ALL, Direction::Out).unwrap(),
            NodeLabel::ALL.map(|l| s.nodes_by_label(l, 1000).unwrap()),
        )
    };
    let before = snapshot(&store);

    // A placeholder with the id of a real symbol: a duplicate primary key
    // after the old edges and nodes were already deleted in the transaction.
    let real = fx.symbol("orders_api.repo.save_order");
    let mut bad = resolved.clone();
    bad.nodes
        .push(NodeRec::bare(&real.id, NodeLabel::Symbol, "x", "x", kind::UNRESOLVED));
    assert!(store.replace_edges(&bad, COMMIT, NOW + 9).is_err());
    assert_eq!(snapshot(&store), before);
    assert_eq!(
        store.nodes(std::slice::from_ref(&real.id)).unwrap(),
        vec![node_of(real)]
    );

    // The same with enough edges for the bulk load and its statement retry.
    let mut big = bad.clone();
    let (a, b) = (
        fx.id("orders_api.routes.create_order"),
        fx.id("orders_api.repo.load_order"),
    );
    for i in 0..2500 {
        big.edges.push(edge(
            &a,
            &b,
            EdgeType::References,
            0.5,
            method::INFERRED,
            "pad.py",
            i,
            i,
        ));
    }
    assert!(store.replace_edges(&big, COMMIT, NOW + 9).is_err());
    assert_eq!(snapshot(&store), before);

    // Usable afterwards.
    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    assert_eq!(snapshot(&store), before);
    store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW).unwrap();
    assert_eq!(stat(&store, "files"), fx.files.len() as i64);
}

#[test]
fn reindexing_a_file_leaves_no_orphans_and_no_duplicates() {
    let (_dir, store, fx, resolved) = indexed();
    let orphans = |s: &Store| {
        (
            count(s, "MATCH (c:CallSite) WHERE NOT EXISTS { MATCH (f:File) WHERE f.path = c.file_path } RETURN count(c);"),
            count(s, "MATCH (i:ImportSite) WHERE NOT EXISTS { MATCH (f:File) WHERE f.path = i.file_path } RETURN count(i);"),
            count(s, "MATCH (x:FactSite) WHERE NOT EXISTS { MATCH (f:File) WHERE f.path = x.file_path } RETURN count(x);"),
            count(s, "MATCH (n:Symbol) WHERE n.kind <> 'unresolved' AND NOT EXISTS { MATCH (:File)-[:CONTAINS]->(n) } RETURN count(n);"),
            count(s, "MATCH (f:File) WHERE NOT EXISTS { MATCH (:Service)-[:CONTAINS]->(f) } RETURN count(f);"),
            count(s, "MATCH (v:Service) WHERE NOT EXISTS { MATCH (:Repository)-[:CONTAINS]->(v) } RETURN count(v);"),
        )
    };
    let shape = |s: &Store| {
        (
            s.stats().unwrap(),
            count(s, "MATCH ()-[r:CONTAINS]->() RETURN count(r);"),
            count(s, "MATCH (v:Service) RETURN count(DISTINCT v.name);"),
            count(s, "MATCH (r:Repository) RETURN count(r);"),
        )
    };
    assert_eq!(orphans(&store), (0, 0, 0, 0, 0, 0));
    let before = shape(&store);

    // delete_file then insert_parsed, for every file, one at a time.
    for p in &fx.files {
        let path = &p.file.as_ref().unwrap().path;
        store.delete_file(path).unwrap();
        assert_eq!(orphans(&store), (0, 0, 0, 0, 0, 0), "after deleting {path}");
        assert!(store.symbols_in_file(path).unwrap().is_empty());
        store.insert_parsed(p, COMMIT, NOW).unwrap();
        assert_eq!(orphans(&store), (0, 0, 0, 0, 0, 0), "after inserting {path}");
    }
    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    assert_eq!(shape(&store), before);

    // A whole service deleted and re-indexed: one Service node again, under the repository.
    for path in [CONSUMER_TS, MAILER_TS, TEMPLATES_TS, AUDIT_TS] {
        store.delete_file(path).unwrap();
    }
    assert!(store.nodes(&[service_id("notifier")]).unwrap().is_empty());
    assert_eq!(stat(&store, "services"), 2);
    // Resolving in between: edges into the deleted service are left out, not failed.
    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    assert!(store.last_skipped_edges() > 0);
    assert_eq!(orphans(&store), (0, 0, 0, 0, 0, 0));
    for path in [CONSUMER_TS, MAILER_TS, TEMPLATES_TS, AUDIT_TS] {
        store.insert_parsed(fx.get(path), COMMIT, NOW).unwrap();
    }
    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    assert_eq!(shape(&store), before);
    assert_eq!(orphans(&store), (0, 0, 0, 0, 0, 0));

    // The bulk path over files that are already indexed, one of them twice in the batch.
    let mut batch = fx.files.clone();
    batch.push(fx.get(ROUTES_PY).clone());
    store.insert_parsed_bulk(&batch, COMMIT, NOW).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    assert_eq!(shape(&store), before);
    assert_eq!(orphans(&store), (0, 0, 0, 0, 0, 0));
}

#[test]
fn a_file_that_shrinks_loses_its_old_rows() {
    let (_dir, store, fx, _) = indexed();
    let mut smaller = fx.get(ROUTES_PY).clone();
    smaller.symbols.truncate(1);
    smaller.calls.clear();
    smaller.imports.clear();
    smaller.facts.clear();
    let before = store.load_resolve_input().unwrap();
    store.insert_parsed(&smaller, COMMIT, NOW).unwrap();
    let after = store.load_resolve_input().unwrap();
    assert_eq!(after.symbols.len(), before.symbols.len() - 2);
    assert_eq!(after.calls.len(), before.calls.len() - 3);
    assert_eq!(after.imports.len(), before.imports.len() - 2);
    assert_eq!(after.facts.len(), before.facts.len() - 2);
    assert!(after.calls.iter().all(|c| c.file_path != ROUTES_PY));
    // Edges that touched the removed symbols are gone, in both directions.
    let gone = fx.id("orders_api.routes.create_order");
    assert!(store
        .edges(std::slice::from_ref(&gone), &EdgeType::ALL, Direction::Out)
        .unwrap()
        .is_empty());
    assert!(store
        .edges(std::slice::from_ref(&gone), &EdgeType::ALL, Direction::In)
        .unwrap()
        .is_empty());
    assert!(store
        .callers(&fx.id("orders_api.repo.save_order"), 6)
        .unwrap()
        .is_empty());
}

#[test]
fn edges_take_empty_huge_mixed_and_unknown_id_lists() {
    let (_dir, store, fx, resolved) = indexed();

    // No ids: nothing, whatever the types.
    for types in [&[][..], &EdgeType::ALL[..], &[EdgeType::Contains][..]] {
        for dir in [Direction::Out, Direction::In] {
            assert!(store.edges(&[], types, dir).unwrap().is_empty());
        }
    }
    assert!(store.nodes(&[]).unwrap().is_empty());

    // Ids of every label at once, with repeats and unknown ids in between.
    let mut mixed: Vec<String> = vec![
        lge_core::repository_id("/repo/polyglot"),
        service_id("orders_api"),
        file_id(ROUTES_PY),
        fx.id("orders_api.routes.create_order"),
        lge_core::endpoint_id("POST", "/orders"),
        lge_core::table_id("orders"),
        lge_core::queue_id("order.created"),
        unresolved_id("render"),
        unresolved_id("GET /x"),
        "no such id".to_string(),
        String::new(),
    ];
    mixed.extend(mixed.clone());
    let nodes = store.nodes(&mixed).unwrap();
    let labels: std::collections::BTreeSet<NodeLabel> = nodes.iter().map(|n| n.label).collect();
    assert_eq!(labels.len(), 7, "{labels:?}");
    assert_eq!(nodes.len(), 9, "unknown ids are skipped and repeats collapse");

    let out = store.edges(&mixed, &[], Direction::Out).unwrap();
    let expected: Vec<EdgeRec> = sorted(
        resolved
            .edges
            .iter()
            .filter(|e| mixed.contains(&e.from))
            .cloned()
            .collect(),
    );
    assert_eq!(sorted(out.clone()), expected);
    assert!(out.iter().all(|e| e.edge_type != EdgeType::Contains));
    let into = store.edges(&mixed, &[], Direction::In).unwrap();
    let expected: Vec<EdgeRec> = sorted(
        resolved
            .edges
            .iter()
            .filter(|e| mixed.contains(&e.to))
            .cloned()
            .collect(),
    );
    assert_eq!(sorted(into), expected);

    // CONTAINS only: the structure, reported as certain.
    let contains = store.edges(&mixed, &[EdgeType::Contains], Direction::Out).unwrap();
    assert!(contains
        .iter()
        .all(|e| e.edge_type == EdgeType::Contains && e.method == method::STATIC && e.confidence == 1.0));
    // repository -> 3 services, service -> 4 files, file -> 3 symbols
    assert_eq!(contains.len(), 3 + 4 + 3);
    let contains_in = store.edges(&mixed, &[EdgeType::Contains], Direction::In).unwrap();
    // service <- repository, file <- service, symbol <- file
    assert_eq!(contains_in.len(), 3);

    // A huge list, mostly unknown ids, with the known ones spread over the chunks.
    let mut huge: Vec<String> = (0..25_000).map(|i| format!("missing-{i}")).collect();
    for (i, e) in resolved.edges.iter().enumerate() {
        huge.insert((i * 800) % huge.len(), e.from.clone());
    }
    let all = store.edges(&huge, &[], Direction::Out).unwrap();
    assert_eq!(sorted(all), sorted(resolved.edges.clone()));
    let known: std::collections::BTreeSet<&String> = resolved.edges.iter().map(|e| &e.from).collect();
    assert_eq!(store.nodes(&huge).unwrap().len(), known.len());
}

#[test]
fn limits_of_zero_and_of_everything_are_accepted() {
    let (_dir, store, fx, _) = indexed();
    store
        .put_handoff(&HandoffNote {
            id: "n1".into(),
            created_at: 5,
            ..HandoffNote::default()
        })
        .unwrap();
    for label in NodeLabel::ALL {
        assert!(store.nodes_by_label(label, 0).unwrap().is_empty());
        assert!(
            !store.nodes_by_label(label, usize::MAX).unwrap().is_empty(),
            "{label:?}"
        );
    }
    assert_eq!(
        store.nodes_by_label(NodeLabel::Symbol, usize::MAX).unwrap().len(),
        fx.symbol_count() + 1
    );
    assert_eq!(store.find_prefix("send", usize::MAX).unwrap().len(), 2);
    assert!(store.find_prefix("send", 0).unwrap().is_empty());
    assert_eq!(
        store.search("create order", usize::MAX).unwrap()[0].name,
        "create_order"
    );
    assert!(store.search("create order", 0).unwrap().is_empty());
    assert_eq!(store.latest_handoffs(usize::MAX).unwrap().len(), 1);
    assert!(store.latest_handoffs(0).unwrap().is_empty());
    // Depth is clamped to 1..=6.
    let save = fx.id("orders_api.repo.save_order");
    assert_eq!(store.callers(&save, 0).unwrap(), store.callers(&save, 1).unwrap());
    assert_eq!(store.callers(&save, 255).unwrap(), store.callers(&save, 6).unwrap());
    assert!(store.callers("no such id", 3).unwrap().is_empty());
    assert_eq!(store.unresolved_out("no such id").unwrap(), 0);
}

#[test]
fn search_handles_empty_stopword_punctuation_and_long_text() {
    let (_dir, store, _fx, _) = indexed();
    for text in [
        "",
        "   ",
        "the and for with",
        "!!! ??? ... ---",
        "a b c",
        "' OR 1=1 --",
        "\"",
        "\\",
        "$q",
        "{x}",
        "\n\t",
    ] {
        assert!(store.search(text, 10).unwrap().is_empty(), "{text:?}");
    }
    // Punctuation around real words does not hide them.
    for text in [
        "create_order!!!",
        "(create) [order]",
        "'create' \"order\"",
        "create\norder",
        "CREATE-ORDER?",
    ] {
        assert_eq!(store.search(text, 10).unwrap()[0].name, "create_order", "{text:?}");
    }
    // A very long task text: the words that match still decide.
    let mut long = String::new();
    for i in 0..3000 {
        long.push_str(&format!("word{i} lorem ipsum, "));
    }
    long.push_str("create order");
    let started = std::time::Instant::now();
    let hits = store.search(&long, 5).unwrap();
    assert!(hits.len() <= 5);
    assert_eq!(hits[0].name, "create_order");
    // And one where nothing matches at all.
    let nothing: String = (0..3000).map(|i| format!("zzqq{i} ")).collect();
    assert!(store.search(&nothing, 5).unwrap().is_empty());
    assert!(started.elapsed().as_secs() < 30, "search took {:?}", started.elapsed());
}

#[test]
fn search_reaches_the_signature_and_folds_case_beyond_ascii() {
    let (_dir, store) = open();
    let f = file("svc/a.py", "svc", lang::PYTHON);
    let mut by_signature = sym(&f, "svc.a.handle", kind::FUNCTION, 1, 5);
    by_signature.signature = "def handle(invoice_payload: Quartz)".to_string();
    let unicode = sym(&f, "svc.a.\u{00dc}bersicht", kind::FUNCTION, 6, 9);
    let p = ParsedFile {
        file: Some(f),
        symbols: vec![by_signature, unicode],
        ..ParsedFile::default()
    };
    store.insert_parsed(&p, COMMIT, NOW).unwrap();

    let names = |text: &str| -> Vec<String> { store.search(text, 5).unwrap().into_iter().map(|h| h.name).collect() };
    assert_eq!(names("quartz"), vec!["handle"], "a word only the signature has");
    assert_eq!(names("\u{00fc}bersicht"), vec!["\u{00dc}bersicht"]);
    assert_eq!(names("\u{00dc}BERSICHT"), vec!["\u{00dc}bersicht"]);
    let prefix = |p: &str| -> Vec<String> { store.find_prefix(p, 5).unwrap().into_iter().map(|h| h.name).collect() };
    assert_eq!(prefix("\u{00fc}ber"), vec!["\u{00dc}bersicht"]);
    assert_eq!(prefix("\u{00dc}BER"), vec!["\u{00dc}bersicht"]);
}

fn usage(id: &str, started_at: i64) -> UsageRecord {
    UsageRecord {
        id: id.to_string(),
        task_id: format!("task-{id}"),
        started_at,
        attempt: 1,
        ..UsageRecord::default()
    }
}

#[test]
fn usage_lists_and_hostile_text_round_trip() {
    let (_dir, store) = open();
    let awkward: Vec<String> = HOSTILE.iter().map(|s| s.to_string()).collect();
    let mut recs = Vec::new();
    for (i, text) in HOSTILE.iter().enumerate() {
        let mut u = usage(&format!("u{i:02}"), 100 + i as i64);
        u.task = text.to_string();
        u.reason = text.to_string();
        u.model = text.to_string();
        u.session_id = text.to_string();
        u.outcome = text.to_string();
        u.files = vec![
            text.to_string(),
            "a,b".to_string(),
            "[x]".to_string(),
            "\"q\"".to_string(),
        ];
        u.services = if i % 2 == 0 { Vec::new() } else { awkward.clone() };
        recs.push(u);
    }
    // Empty lists, a list of one empty string, and extreme numbers.
    let mut empty = usage("empty", 50);
    empty.files = Vec::new();
    empty.services = vec![String::new()];
    empty.input_tokens = i64::MAX;
    empty.cached_input_tokens = i64::MIN;
    empty.attempt = -1;
    empty.escalated = true;
    recs.insert(0, empty);
    // Inserted newest first: the order comes from the store, not from the insert.
    for u in recs.iter().rev() {
        store.put_usage(u).unwrap();
    }
    assert_eq!(store.list_usage(i64::MIN).unwrap(), recs);
    assert_eq!(store.list_usage(0).unwrap(), recs);
    assert_eq!(store.list_usage(100).unwrap(), recs[1..]);
    assert_eq!(store.list_usage(101).unwrap(), recs[2..]);
    assert!(store.list_usage(100 + HOSTILE.len() as i64).unwrap().is_empty());
    assert!(store.list_usage(i64::MAX).unwrap().is_empty());

    // Same id: replaced, lists included (a filled list becomes empty and back).
    let mut replaced = recs[2].clone();
    replaced.files = Vec::new();
    replaced.services = vec!["one".to_string()];
    replaced.started_at = 10;
    store.put_usage(&replaced).unwrap();
    let listed = store.list_usage(0).unwrap();
    assert_eq!(listed.len(), recs.len());
    assert_eq!(listed[0], replaced);
    store.put_usage(&recs[2]).unwrap();
    assert_eq!(store.list_usage(0).unwrap(), recs);

    // Handoff notes with the same text; ties on the time are broken by id.
    for (i, text) in HOSTILE.iter().enumerate() {
        store
            .put_handoff(&HandoffNote {
                id: format!("n{i:02}"),
                task_id: text.to_string(),
                task: text.to_string(),
                created_at: 7,
                text: text.to_string(),
            })
            .unwrap();
    }
    let notes = store.latest_handoffs(100).unwrap();
    assert_eq!(notes.len(), HOSTILE.len());
    for (i, n) in notes.iter().rev().enumerate() {
        assert_eq!(
            (n.id.as_str(), n.text.as_str(), n.task.as_str()),
            (format!("n{i:02}").as_str(), HOSTILE[i], HOSTILE[i])
        );
    }
    // Replaced by id, and the newest comes first.
    store
        .put_handoff(&HandoffNote {
            id: "n03".into(),
            created_at: 9,
            text: "new".into(),
            ..HandoffNote::default()
        })
        .unwrap();
    let notes = store.latest_handoffs(2).unwrap();
    assert_eq!(notes.len(), 2);
    assert_eq!((notes[0].id.as_str(), notes[0].text.as_str()), ("n03", "new"));
    assert_eq!(store.latest_handoffs(100).unwrap().len(), HOSTILE.len());
}

#[test]
fn large_text_is_not_cut() {
    let (_dir, store) = open();
    let big: String = "abcdefghij".repeat(40_000); // 400 kB
    let f = file("svc/big.py", "svc", lang::PYTHON);
    let mut s = sym(&f, "svc.big.f", kind::FUNCTION, 1, 5);
    s.body = big.clone();
    s.doc = big.clone();
    let p = ParsedFile {
        file: Some(f),
        symbols: vec![s.clone()],
        ..ParsedFile::default()
    };
    store.insert_parsed(&p, COMMIT, NOW).unwrap();
    assert_eq!(store.symbol_body(&s.id).unwrap().map(|b| b.len()), Some(big.len()));
    assert_eq!(
        store.nodes(std::slice::from_ref(&s.id)).unwrap()[0].doc.len(),
        big.len()
    );
    store
        .put_handoff(&HandoffNote {
            id: "n".into(),
            text: big.clone(),
            ..HandoffNote::default()
        })
        .unwrap();
    assert_eq!(store.latest_handoffs(1).unwrap()[0].text.len(), big.len());
    let mut u = usage("u", 1);
    u.task = big.clone();
    store.put_usage(&u).unwrap();
    assert_eq!(store.list_usage(0).unwrap()[0].task.len(), big.len());
}

#[test]
fn a_database_file_is_open_once_per_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let fx = fixture();
    let first = Store::open(&path).unwrap();
    first.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW).unwrap();

    // LadybugDB would let a second handle in, and the two would not see each
    // other's commits. The store refuses, however the path is spelled.
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let spellings = [
        path.clone(),
        dir.path().join("sub").join("..").join("graph.lbug"),
        dir.path().join(".").join("graph.lbug"),
    ];
    for spelling in &spellings {
        for attempt in [Store::open(spelling), Store::open_without_fts(spelling)] {
            let err = attempt
                .err()
                .unwrap_or_else(|| panic!("{} was opened twice", spelling.display()));
            assert!(format!("{err:#}").contains("already open"), "{err:#}");
        }
    }
    #[cfg(unix)]
    {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        assert!(
            Store::open(&link.join("graph.lbug")).is_err(),
            "through a symlinked directory"
        );
    }

    // The refused attempts changed nothing and the first handle still works.
    first.insert_parsed(fx.get(REPO_PY), COMMIT, NOW).unwrap();
    assert_eq!(stat(&first, "files"), 2);

    // Another file is another database.
    let other = Store::open(&dir.path().join("other.lbug")).unwrap();
    assert_eq!(stat(&other, "files"), 0);

    // Closed, it can be opened again, by any spelling, with everything in it.
    drop(first);
    let again = Store::open(&spellings[1]).unwrap();
    assert_eq!(stat(&again, "files"), 2);
    assert!(again.file_info(ROUTES_PY).unwrap().is_some());
    drop(again);

    // An open that fails (a newer schema) does not keep the file claimed.
    {
        let store = Store::open(&path).unwrap();
        store
            .raw("MATCH (m:Meta) WHERE m.key = 'schema_version' SET m.value = '99';")
            .unwrap();
    }
    assert!(Store::open(&path).is_err());
    let err = Store::open(&path).unwrap_err();
    assert!(format!("{err:#}").contains("schema version"), "{err:#}");
}

/// Not a test of its own: `another_process_cannot_open_the_database` runs it
/// in a second process, with the path of a database the first one holds open.
#[test]
fn child_tries_to_open_the_database() {
    let Ok(path) = std::env::var("LGE_STORE_TEST_CHILD_DB") else {
        return;
    };
    match Store::open(std::path::Path::new(&path)) {
        Ok(store) => println!("CHILD OPENED files={}", stat(&store, "files")),
        Err(e) => println!("CHILD REFUSED {e:#}"),
    }
}

#[test]
fn another_process_cannot_open_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let store = Store::open(&path).unwrap();
    store.insert_parsed(fixture().get(ROUTES_PY), COMMIT, NOW).unwrap();

    let child = |path: &std::path::Path| {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "child_tries_to_open_the_database",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LGE_STORE_TEST_CHILD_DB", path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let while_open = child(&path);
    assert!(while_open.contains("CHILD REFUSED"), "{while_open}");

    // The refused process damaged nothing, and once the owner is gone the file is free.
    store.insert_parsed(fixture().get(REPO_PY), COMMIT, NOW).unwrap();
    assert_eq!(stat(&store, "files"), 2);
    drop(store);
    let after_close = child(&path);
    assert!(after_close.contains("CHILD OPENED files=2"), "{after_close}");
    assert_eq!(stat(&Store::open(&path).unwrap(), "files"), 2);
}

#[test]
fn schema_versions_below_the_current_one_are_migrated() {
    for old in ["0", "1", "2", "-5", " 1 "] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.lbug");
        let note = HandoffNote {
            id: "n1".into(),
            task_id: "t".into(),
            task: "it's \"quoted\"".into(),
            created_at: 3,
            text: "kept, with a\nline break".into(),
        };
        let mut u = usage("u1", 4);
        u.files = vec!["a,b.py".into(), String::new()];
        {
            let store = Store::open(&path).unwrap();
            store.insert_parsed(fixture().get(ROUTES_PY), COMMIT, NOW).unwrap();
            store.put_handoff(&note).unwrap();
            store.put_usage(&u).unwrap();
            store
                .raw(&format!(
                    "MATCH (m:Meta) WHERE m.key = 'schema_version' SET m.value = '{old}';"
                ))
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            stat(&store, "files"),
            0,
            "version {old:?}: the graph is rebuilt by the next index run"
        );
        assert_eq!(store.latest_handoffs(5).unwrap(), vec![note], "version {old:?}");
        assert_eq!(store.list_usage(0).unwrap(), vec![u], "version {old:?}");
        assert_eq!(
            count(
                &store,
                "MATCH (m:Meta) WHERE m.key = 'schema_version' AND m.value = '3' RETURN count(m);"
            ),
            1
        );
    }
}

#[test]
fn provenance_is_stored_as_given_for_every_edge_type() {
    let (_dir, store, _fx, resolved) = indexed();
    // Every edge of the fixture again, once per method, with a confidence no
    // rule of the resolver would produce: the store must not correct either.
    let mut odd = Resolved {
        nodes: resolved.nodes.clone(),
        ..Resolved::default()
    };
    let methods = [method::STATIC, method::INFERRED, method::UNRESOLVED];
    let confidences = [0.123_456_789, 1.0, 0.0, 0.999_999_999_999, 0.5, 7.25];
    let mut seen = std::collections::HashSet::new();
    for e in &resolved.edges {
        if !seen.insert((e.from.clone(), e.to.clone(), e.edge_type)) {
            continue;
        }
        let i = odd.edges.len();
        odd.edges.push(EdgeRec {
            method: methods[i % 3].to_string(),
            confidence: confidences[i % confidences.len()],
            evidence_file: format!("evidence/{i}.py"),
            start_line: i as i64 * 3,
            end_line: i as i64 * 3 + 1,
            ..e.clone()
        });
    }
    let types: std::collections::BTreeSet<EdgeType> = odd.edges.iter().map(|e| e.edge_type).collect();
    assert_eq!(types.len(), 13, "every type except CONTAINS");
    store.replace_edges(&odd, COMMIT, NOW).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    let from: Vec<String> = odd.edges.iter().map(|e| e.from.clone()).collect();
    assert_eq!(
        sorted(store.edges(&from, &[], Direction::Out).unwrap()),
        sorted(odd.edges.clone())
    );
    let to: Vec<String> = odd.edges.iter().map(|e| e.to.clone()).collect();
    assert_eq!(
        sorted(store.edges(&to, &[], Direction::In).unwrap()),
        sorted(odd.edges.clone())
    );

    // An inferred or unresolved edge is counted as what it is, never as static.
    for (t, m) in [
        (EdgeType::Calls, method::UNRESOLVED),
        (EdgeType::Imports, method::INFERRED),
        (EdgeType::DependsOn, method::UNRESOLVED),
    ] {
        let given = odd.edges.iter().filter(|e| e.edge_type == t && e.method == m).count() as i64;
        let stored = count(
            &store,
            &format!(
                "MATCH ()-[r:{}]->() WHERE r.method = '{m}' RETURN count(r);",
                t.as_str()
            ),
        );
        assert_eq!(stored, given, "{t} {m}");
    }
}

/// The fixture, indexed and resolved, in a store that did not load fts.
fn indexed_without_fts() -> (tempfile::TempDir, Store, Fixture, Resolved) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_without_fts(&dir.path().join("graph.lbug")).unwrap();
    assert!(!store.fts_enabled());
    let fx = fixture();
    store.set_repository("/repo/polyglot", COMMIT).unwrap();
    store.insert_parsed_bulk(&fx.files, COMMIT, NOW).unwrap();
    let resolved = resolved(&fx);
    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    (dir, store, fx, resolved)
}

fn hit_names(store: &Store, text: &str, limit: usize) -> Vec<String> {
    let hits = store.search(text, limit).unwrap();
    assert!(hits.len() <= limit);
    assert!(
        hits.windows(2).all(|w| w[0].score >= w[1].score),
        "{text}: not ordered by score"
    );
    assert!(hits.iter().all(|h| h.score > 0.0), "{text}: a hit without a score");
    assert!(
        hits.iter()
            .all(|h| h.kind != kind::MODULE && h.kind != kind::UNRESOLVED),
        "{text}"
    );
    hits.into_iter().map(|h| h.name).collect()
}

#[test]
fn without_fts_search_is_a_substring_match_over_the_same_columns() {
    let (_dir, store, _fx, _) = indexed_without_fts();
    // No full-text index was created behind the missing extension.
    assert!(!store
        .raw("CALL SHOW_INDEXES() RETURN index_type;")
        .unwrap()
        .contains("FTS"));

    assert_eq!(hit_names(&store, "create order", 10)[0], "create_order");
    assert!(hit_names(&store, "create order", 10).contains(&"test_create_order".to_string()));
    for query in ["recordDelivery", "RECORDDELIVERY", "please fix recordDelivery"] {
        assert_eq!(hit_names(&store, query, 5), vec!["recordDelivery"], "{query}");
    }
    // doc, qualified name, and a part of a camelCase name
    assert_eq!(hit_names(&store, "confirmation", 5), vec!["handleOrderCreated"]);
    assert!(hit_names(&store, "mailer", 20).contains(&"sendEmail".to_string()));
    assert!(hit_names(&store, "handler", 20).contains(&"getOrderHandler".to_string()));
    // The signature is searched too: `payload` is only in `def create_order(payload)`.
    assert_eq!(hit_names(&store, "payload", 5), vec!["create_order"]);
    assert_eq!(hit_names(&store, "render", 10), vec!["render", "render"]);

    for text in ["", "the and for", "zzzqqqxxx", "!!!", "' OR 1=1 --"] {
        assert!(hit_names(&store, text, 10).is_empty(), "{text:?}");
    }
    assert!(hit_names(&store, "create order", 0).is_empty());
    assert_eq!(hit_names(&store, "order", 2).len(), 2);
    assert_eq!(hit_names(&store, "create order", usize::MAX)[0], "create_order");

    // A very long text costs a bounded number of scans and still finds its words.
    let mut long = String::from("create order ");
    for i in 0..5000 {
        long.push_str(&format!("filler{i} "));
    }
    let started = std::time::Instant::now();
    assert_eq!(hit_names(&store, &long, 3)[0], "create_order");
    assert!(started.elapsed().as_secs() < 20, "{:?}", started.elapsed());

    // Lookups and traversals do not depend on the extension.
    assert_eq!(store.find("create_order").unwrap().len(), 1);
    assert_eq!(store.find_prefix("send", 10).unwrap().len(), 2);
}

#[test]
fn without_fts_case_is_folded_beyond_ascii() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_without_fts(&dir.path().join("graph.lbug")).unwrap();
    let f = file("svc/a.py", "svc", lang::PYTHON);
    let mut unicode = sym(&f, "svc.a.\u{00dc}bersicht", kind::FUNCTION, 6, 9);
    unicode.doc = "Gr\u{00d6}\u{00df}e \u{0416}\u{0423}\u{041a}".to_string();
    let p = ParsedFile {
        file: Some(f),
        symbols: vec![unicode],
        ..ParsedFile::default()
    };
    store.insert_parsed(&p, COMMIT, NOW).unwrap();
    for text in [
        "\u{00fc}bersicht",
        "\u{00dc}BERSICHT",
        "gr\u{00f6}\u{00df}e",
        "\u{0436}\u{0443}\u{043a}",
    ] {
        assert_eq!(hit_names(&store, text, 5), vec!["\u{00dc}bersicht"], "{text}");
    }
    for prefix in ["\u{00fc}ber", "\u{00dc}BER"] {
        assert_eq!(store.find_prefix(prefix, 5).unwrap().len(), 1, "{prefix}");
    }
}

#[test]
fn an_index_without_its_extension_refuses_graph_writes_and_keeps_reading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let fx = fixture();
    let resolved = resolved(&fx);
    {
        let store = Store::open(&path).unwrap();
        if !store.fts_enabled() {
            // Without the extension on this machine there is no index to be left behind.
            return;
        }
        store.set_repository("/repo/polyglot", COMMIT).unwrap();
        store.insert_parsed_bulk(&fx.files, COMMIT, NOW).unwrap();
        store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    }

    // The same database in a process that cannot load fts.
    let store = Store::open_without_fts(&path).unwrap();
    assert!(!store.fts_enabled());
    // Everything can be read, and search answers by substring.
    assert_eq!(stat(&store, "files"), fx.files.len() as i64);
    assert_eq!(hit_names(&store, "create order", 5)[0], "create_order");
    let from: Vec<String> = resolved.edges.iter().map(|e| e.from.clone()).collect();
    assert_eq!(
        sorted(store.edges(&from, &[], Direction::Out).unwrap()),
        sorted(resolved.edges.clone())
    );
    assert_eq!(store.callers(&fx.id("orders_api.repo.save_order"), 2).unwrap().len(), 2);
    // Graph writes are refused with a message that says why, and change nothing.
    for err in [
        store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW).unwrap_err(),
        store.insert_parsed_bulk(&fx.files, COMMIT, NOW).unwrap_err(),
        store.delete_file(ROUTES_PY).unwrap_err(),
        store.replace_edges(&resolved, COMMIT, NOW).unwrap_err(),
    ] {
        assert!(format!("{err:#}").contains("fts extension is not loaded"), "{err:#}");
    }
    assert_eq!(stat(&store, "files"), fx.files.len() as i64);
    assert_eq!(
        sorted(store.edges(&from, &[], Direction::Out).unwrap()),
        sorted(resolved.edges.clone())
    );
    // The usage log, the handoff notes and the run log are not graph writes.
    store.put_usage(&usage("u1", 5)).unwrap();
    store
        .put_handoff(&HandoffNote {
            id: "n1".into(),
            ..HandoffNote::default()
        })
        .unwrap();
    let run = store.begin_run(NOW).unwrap();
    store.finish_run(&run, NOW + 1, "failed", 0).unwrap();
    assert_eq!(store.list_usage(0).unwrap().len(), 1);
    drop(store);

    // With the extension back, the same database is writable again.
    let store = Store::open(&path).unwrap();
    assert!(store.fts_enabled());
    store.delete_file(ROUTES_PY).unwrap();
    store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    assert_eq!(store.list_usage(0).unwrap().len(), 1);
}

#[test]
fn a_database_indexed_without_fts_gets_its_index_when_the_extension_appears() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let (_d, without, fx, resolved) = indexed_without_fts();
    drop(without);
    {
        let store = Store::open_without_fts(&path).unwrap();
        store.insert_parsed_bulk(&fx.files, COMMIT, NOW).unwrap();
        store.replace_edges(&resolved, COMMIT, NOW).unwrap();
        assert_eq!(hit_names(&store, "create order", 5)[0], "create_order");
    }
    let store = Store::open(&path).unwrap();
    if !store.fts_enabled() {
        return;
    }
    // Symbols stored before the index existed are in it.
    let indexed = store
        .raw("CALL QUERY_FTS_INDEX('Symbol', 'sym_fts', 'confirmation') RETURN node.name;")
        .unwrap();
    assert!(indexed.contains("handleOrderCreated"), "{indexed}");
    assert_eq!(hit_names(&store, "confirmation", 5), vec!["handleOrderCreated"]);
    store.delete_file(ROUTES_PY).unwrap();
    assert!(hit_names(&store, "announce", 5).is_empty());
}

#[test]
fn readers_never_see_a_half_replaced_graph() {
    let (_dir, store, _fx, resolved) = indexed();
    let from: Vec<String> = resolved.edges.iter().map(|e| e.from.clone()).collect();
    let expected = sorted(resolved.edges.clone());
    let derived = resolved.nodes.len();
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            for i in 0..15 {
                store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
                if i % 5 == 0 {
                    store.insert_parsed(fixture().get(VIEWS_TS), COMMIT, NOW).unwrap();
                    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
                }
            }
            done.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let mut reads = 0;
        while !done.load(std::sync::atomic::Ordering::Relaxed) {
            let edges = store.edges(&from, &[], Direction::Out).unwrap();
            // Between the insert of views.ts and the resolve that follows it,
            // the edges into its symbols are gone; nothing else may be missing.
            let missing: Vec<&EdgeRec> = expected.iter().filter(|e| !edges.contains(e)).collect();
            assert!(
                missing
                    .iter()
                    .all(|e| e.evidence_file == ROUTES_TS && e.edge_type == EdgeType::References),
                "a reader saw a graph with {} of {} edges",
                edges.len(),
                expected.len()
            );
            let nodes: usize = [NodeLabel::ApiEndpoint, NodeLabel::DbTable, NodeLabel::Queue]
                .iter()
                .map(|l| store.nodes_by_label(*l, 100).unwrap().len())
                .sum();
            assert_eq!(nodes + 1, derived, "derived nodes seen by a reader");
            reads += 1;
        }
        writer.join().unwrap();
        assert!(reads > 0);
    });
    assert_eq!(sorted(store.edges(&from, &[], Direction::Out).unwrap()), expected);
}
