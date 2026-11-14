//! The resolver's output: derived nodes, edges with provenance, and the
//! traversals that read them.

mod common;

use common::*;
use lge_core::{
    file_id, kind, lang, method, repository_id, service_id, unresolved_id, Direction, EdgeRec, EdgeType, NodeLabel,
    NodeRec, ParsedFile, Resolved,
};
use lge_store::{GraphStore, Store};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

fn of_type(resolved: &Resolved, t: EdgeType) -> Vec<EdgeRec> {
    sorted(resolved.edges.iter().filter(|e| e.edge_type == t).cloned().collect())
}

/// The data rows of a table printed by `raw`.
fn raw_rows(store: &Store, cypher: &str) -> Vec<String> {
    store
        .raw(cypher)
        .unwrap()
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn every_edge_type_keeps_its_provenance() {
    let (_dir, store, _fx, resolved) = indexed();
    assert_eq!(store.last_skipped_edges(), 0);

    for t in EdgeType::ALL.into_iter().filter(|t| *t != EdgeType::Contains) {
        let expected = of_type(&resolved, t);
        assert!(!expected.is_empty(), "the fixture has no {t} edge");

        // Confidence, method, evidence file and both lines come back exactly.
        let from: Vec<String> = expected.iter().map(|e| e.from.clone()).collect();
        assert_eq!(
            sorted(store.edges(&from, &[t], Direction::Out).unwrap()),
            expected,
            "{t} out"
        );
        let to: Vec<String> = expected.iter().map(|e| e.to.clone()).collect();
        assert_eq!(
            sorted(store.edges(&to, &[t], Direction::In).unwrap()),
            expected,
            "{t} in"
        );

        // Commit and creation time are stored on every edge of every table.
        let stamps = raw_rows(
            &store,
            &format!("MATCH ()-[r:{t}]->() RETURN DISTINCT r.commit, r.created_at;"),
        );
        assert_eq!(stamps, vec![format!("{COMMIT}|{}", NOW + 5)], "{t}");
        let total = raw_rows(&store, &format!("MATCH ()-[r:{t}]->() RETURN count(r);"));
        assert_eq!(total, vec![expected.len().to_string()], "{t}");
    }

    // All three methods are present and none was changed on the way in.
    let all = store
        .edges(
            &resolved.edges.iter().map(|e| e.from.clone()).collect::<Vec<_>>(),
            &[],
            Direction::Out,
        )
        .unwrap();
    assert_eq!(sorted(all.clone()), sorted(resolved.edges.clone()));
    let methods: BTreeSet<&str> = all.iter().map(|e| e.method.as_str()).collect();
    assert_eq!(
        methods,
        BTreeSet::from([method::INFERRED, method::STATIC, method::UNRESOLVED])
    );
    assert!(all
        .iter()
        .filter(|e| e.method == method::UNRESOLVED)
        .all(|e| e.confidence == 0.0));
}

#[test]
fn edges_follow_both_directions_and_type_filters() {
    let (_dir, store, fx, resolved) = indexed();
    let create_order = fx.id("orders_api.routes.create_order");
    let types = |edges: &[EdgeRec]| edges.iter().map(|e| e.edge_type).collect::<Vec<_>>();

    // No type given: every type except CONTAINS.
    let out = store
        .edges(std::slice::from_ref(&create_order), &[], Direction::Out)
        .unwrap();
    assert_eq!(
        types(&out),
        vec![
            EdgeType::Calls,
            EdgeType::Calls,
            EdgeType::ExposesApi,
            EdgeType::TestedBy
        ]
    );
    assert!(out.iter().all(|e| e.from == create_order));
    let into = store
        .edges(std::slice::from_ref(&create_order), &[], Direction::In)
        .unwrap();
    assert_eq!(types(&into), vec![EdgeType::Calls]);
    assert_eq!(into[0].from, fx.id("orders_api.tests.test_routes.test_create_order"));
    assert_eq!(into[0].to, create_order);

    // A subset of types, CONTAINS only when asked for.
    let calls = store
        .edges(std::slice::from_ref(&create_order), &[EdgeType::Calls], Direction::Out)
        .unwrap();
    assert_eq!(calls.len(), 2);
    let api = store
        .edges(
            std::slice::from_ref(&create_order),
            &[EdgeType::ExposesApi, EdgeType::CallsApi],
            Direction::Out,
        )
        .unwrap();
    assert_eq!(types(&api), vec![EdgeType::ExposesApi]);
    let both = store
        .edges(
            std::slice::from_ref(&create_order),
            &[EdgeType::Contains, EdgeType::Calls],
            Direction::In,
        )
        .unwrap();
    assert_eq!(types(&both), vec![EdgeType::Contains, EdgeType::Calls]);
    assert_eq!(both[0].from, file_id(ROUTES_PY));
    assert!(store
        .edges(
            std::slice::from_ref(&create_order),
            &[EdgeType::Publishes],
            Direction::Out
        )
        .unwrap()
        .is_empty());

    // Into derived nodes.
    let orders = table("orders").id;
    let touching = store.edges(std::slice::from_ref(&orders), &[], Direction::In).unwrap();
    assert_eq!(types(&touching), vec![EdgeType::ReadsFrom, EdgeType::WritesTo]);
    let writers = store
        .edges(std::slice::from_ref(&orders), &[EdgeType::WritesTo], Direction::In)
        .unwrap();
    assert_eq!(
        writers.iter().map(|e| e.from.clone()).collect::<Vec<_>>(),
        vec![fx.id("orders_api.repo.save_order")]
    );
    assert!(store.edges(&[orders], &[], Direction::Out).unwrap().is_empty());
    let post_orders = endpoint("POST", "/orders", "orders_api").id;
    let users = store.edges(&[post_orders], &[], Direction::In).unwrap();
    assert_eq!(types(&users), vec![EdgeType::ExposesApi, EdgeType::CallsApi]);
    let queue_id = queue("order.created", "queue").id;
    assert_eq!(
        types(&store.edges(&[queue_id], &[], Direction::In).unwrap()),
        vec![EdgeType::Publishes, EdgeType::Consumes]
    );

    // Ids of different labels in one call, repeated ids and unknown ids.
    let mixed = ids(&[
        &file_id(ROUTES_PY),
        &service_id("notifier"),
        &create_order,
        &create_order,
        "no-such-id",
    ]);
    let out = store.edges(&mixed, &[], Direction::Out).unwrap();
    assert_eq!(
        types(&out),
        vec![
            EdgeType::Imports,
            EdgeType::Imports,
            EdgeType::Calls,
            EdgeType::Calls,
            EdgeType::ExposesApi,
            EdgeType::TestedBy,
            EdgeType::DependsOn
        ]
    );
    assert!(store
        .edges(&ids(&["no-such-id"]), &[], Direction::Out)
        .unwrap()
        .is_empty());
    assert!(store.edges(&[], &[], Direction::In).unwrap().is_empty());

    // Looking from the targets finds the same edges as looking from the sources.
    let from: Vec<String> = resolved.edges.iter().map(|e| e.from.clone()).collect();
    let to: Vec<String> = resolved.edges.iter().map(|e| e.to.clone()).collect();
    assert_eq!(
        store.edges(&from, &[], Direction::Out).unwrap(),
        store.edges(&to, &[], Direction::In).unwrap()
    );
}

/// `n` functions in files of 50, each calling the next one.
fn chain(n: usize) -> (Vec<ParsedFile>, Resolved, Vec<String>) {
    let mut files = Vec::new();
    let mut symbol_ids = Vec::new();
    for (i, block) in (0..n).collect::<Vec<_>>().chunks(50).enumerate() {
        let f = file(&format!("big/part_{i}.py"), "big", lang::PYTHON);
        let symbols: Vec<_> = block
            .iter()
            .map(|j| {
                sym(
                    &f,
                    &format!("big.part_{i}.step_{j}"),
                    kind::FUNCTION,
                    *j as i64,
                    *j as i64,
                )
            })
            .collect();
        symbol_ids.extend(symbols.iter().map(|s| s.id.clone()));
        files.push(parsed(f, symbols));
    }
    let edges = symbol_ids
        .windows(2)
        .enumerate()
        .map(|(i, w)| {
            edge(
                &w[0],
                &w[1],
                EdgeType::Calls,
                0.95,
                method::STATIC,
                "big/part.py",
                i as i64,
                i as i64,
            )
        })
        .collect();
    (
        files,
        Resolved {
            edges,
            ..Resolved::default()
        },
        symbol_ids,
    )
}

#[test]
fn edges_and_nodes_take_thousands_of_ids() {
    let (_dir, store) = open();
    let (files, resolved, symbol_ids) = chain(5000);
    store.insert_parsed_bulk(&files, COMMIT, NOW).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    assert_eq!(store.last_bulk_load_error(), None);

    // Unknown ids in between and every id twice: neither adds nor loses a row.
    let mut asked = Vec::new();
    for (i, id) in symbol_ids.iter().enumerate() {
        asked.push(id.clone());
        asked.push(format!("unknown-{i}"));
        asked.push(id.clone());
    }
    let out = store.edges(&asked, &[], Direction::Out).unwrap();
    assert_eq!(sorted(out), sorted(resolved.edges.clone()));
    let into = store.edges(&asked, &[EdgeType::Calls], Direction::In).unwrap();
    assert_eq!(sorted(into), sorted(resolved.edges.clone()));
    let contained = store.edges(&asked, &[EdgeType::Contains], Direction::In).unwrap();
    assert_eq!(contained.len(), 5000);

    let nodes = store.nodes(&asked).unwrap();
    assert_eq!(nodes.len(), 5000);
    let found: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    assert!(symbol_ids.iter().all(|id| found.contains(id.as_str())));

    // The cap on depth holds on a long chain: six hops back, no more.
    let callers = store.callers(&symbol_ids[100], 6).unwrap();
    let expected: Vec<(String, f64)> = (1..=6).map(|d| (symbol_ids[100 - d].clone(), d as f64)).collect();
    assert_eq!(
        callers.iter().map(|h| (h.id.clone(), h.score)).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        store.callers(&symbol_ids[100], 200).unwrap().len(),
        6,
        "depth is capped at 6"
    );
    assert_eq!(
        store.callers(&symbol_ids[100], 0).unwrap().len(),
        1,
        "depth is at least 1"
    );
}

#[test]
fn replace_edges_twice_leaves_the_same_graph() {
    let (_dir, store, fx, resolved) = indexed();
    let all_ids: Vec<String> = resolved
        .edges
        .iter()
        .flat_map(|e| [e.from.clone(), e.to.clone()])
        .collect();
    let snapshot = |store: &Store| {
        (
            store.stats().unwrap(),
            store.edges(&all_ids, &[], Direction::Out).unwrap(),
            NodeLabel::ALL.map(|l| store.nodes_by_label(l, 1000).unwrap()),
        )
    };
    let first = snapshot(&store);

    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    assert_eq!(snapshot(&store), first);
    assert_eq!(store.last_skipped_edges(), 0);

    // The input order does not matter either.
    let mut shuffled = resolved.clone();
    shuffled.edges.reverse();
    shuffled.nodes.reverse();
    store.replace_edges(&shuffled, COMMIT, NOW + 5).unwrap();
    assert_eq!(snapshot(&store), first);

    // Replacing with nothing removes every derived node and edge, and only those.
    store.replace_edges(&Resolved::default(), COMMIT, NOW + 9).unwrap();
    assert!(store.edges(&all_ids, &[], Direction::Out).unwrap().is_empty());
    for label in [NodeLabel::ApiEndpoint, NodeLabel::DbTable, NodeLabel::Queue] {
        assert!(store.nodes_by_label(label, 10).unwrap().is_empty(), "{label:?}");
    }
    assert!(store.nodes(&[unresolved_id("render")]).unwrap().is_empty());
    assert_eq!(stat(&store, "symbols"), fx.symbol_count() as i64);
    assert_eq!(stat(&store, "files"), fx.files.len() as i64);
    assert_eq!(
        store.nodes_by_label(NodeLabel::Symbol, 1000).unwrap().len(),
        fx.symbol_count()
    );
    assert_eq!(
        store
            .edges(&[file_id(ROUTES_PY)], &[EdgeType::Contains], Direction::Out)
            .unwrap()
            .len(),
        3
    );
    let after: BTreeMap<String, i64> = store.stats().unwrap().into_iter().collect();
    assert_eq!(
        (
            after["calls_static"],
            after["calls_inferred"],
            after["calls_unresolved"],
            after["imports"]
        ),
        (0, 0, 0, 0)
    );
    assert_eq!((after["endpoints"], after["tables"], after["queues"]), (0, 0, 0));
}

#[test]
fn an_edge_without_both_endpoints_is_skipped() {
    let (_dir, store, fx, resolved) = indexed();
    let create_order = fx.id("orders_api.routes.create_order");
    let save_order = fx.id("orders_api.repo.save_order");
    let mut with_strays = resolved.clone();
    with_strays.edges.extend([
        // target does not exist
        edge(
            &create_order,
            "missing-symbol",
            EdgeType::Calls,
            0.5,
            method::INFERRED,
            ROUTES_PY,
            1,
            1,
        ),
        // source does not exist
        edge(
            "missing-symbol",
            &save_order,
            EdgeType::Calls,
            0.5,
            method::INFERRED,
            ROUTES_PY,
            1,
            1,
        ),
        // the target exists, but it is not a table
        edge(
            &create_order,
            &save_order,
            EdgeType::ReadsFrom,
            0.9,
            method::STATIC,
            ROUTES_PY,
            1,
            1,
        ),
        // a file cannot call
        edge(
            &file_id(ROUTES_PY),
            &save_order,
            EdgeType::Calls,
            0.9,
            method::STATIC,
            ROUTES_PY,
            1,
            1,
        ),
        // CONTAINS belongs to the indexer
        edge(
            &file_id(ROUTES_PY),
            &save_order,
            EdgeType::Contains,
            1.0,
            method::STATIC,
            ROUTES_PY,
            1,
            1,
        ),
    ]);
    store.replace_edges(&with_strays, COMMIT, NOW).unwrap();
    assert_eq!(store.last_skipped_edges(), 5);

    // Everything else is stored, and nothing was invented for the strays.
    let from: Vec<String> = with_strays.edges.iter().map(|e| e.from.clone()).collect();
    assert_eq!(
        sorted(store.edges(&from, &[], Direction::Out).unwrap()),
        sorted(resolved.edges.clone())
    );
    assert!(store.nodes(&ids(&["missing-symbol"])).unwrap().is_empty());
    assert_eq!(
        store
            .edges(&[save_order], &[EdgeType::Contains], Direction::In)
            .unwrap()
            .len(),
        1
    );

    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
}

#[test]
fn input_that_breaks_a_rule_is_refused_and_changes_nothing() {
    let (_dir, store, fx, resolved) = indexed();
    let before = store.stats().unwrap();
    let create_order = fx.symbol("orders_api.routes.create_order");

    // An edge must say how it was found.
    for bad_method in ["", "guess", "STATIC"] {
        let mut bad = resolved.clone();
        bad.edges.push(edge(
            &create_order.id,
            &fx.id("orders_api.repo.load_order"),
            EdgeType::Calls,
            0.9,
            bad_method,
            ROUTES_PY,
            1,
            1,
        ));
        let err = store.replace_edges(&bad, COMMIT, NOW).unwrap_err();
        assert!(err.to_string().contains("unknown method"), "{err}");
        assert_eq!(store.stats().unwrap(), before);
    }

    // Only derived nodes are written here: not real symbols, files or services.
    for node in [
        node_of(create_order),
        NodeRec::bare("f", NodeLabel::File, "a.py", "a.py", lang::PYTHON),
        NodeRec::bare("s", NodeLabel::Service, "svc", "svc", ""),
        NodeRec::bare("r", NodeLabel::Repository, "/repo", "/repo", "abc"),
    ] {
        let mut bad = resolved.clone();
        bad.nodes.push(node);
        let err = store.replace_edges(&bad, COMMIT, NOW).unwrap_err();
        assert!(err.to_string().contains("not a derived node"), "{err}");
        assert_eq!(store.stats().unwrap(), before);
    }
    assert_eq!(store.symbols_in_file(ROUTES_PY).unwrap().len(), 3);
}

#[test]
fn a_node_derived_twice_is_written_once() {
    let (_dir, store, _fx, mut resolved) = indexed();
    resolved.nodes.push(table("orders"));
    resolved.nodes.push(placeholder("render"));
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    assert_eq!(store.nodes_by_label(NodeLabel::DbTable, 10).unwrap().len(), 2);
    assert_eq!(store.nodes(&[unresolved_id("render")]).unwrap().len(), 1);
}

#[test]
fn nodes_of_every_label_have_the_common_shape() {
    let (_dir, store, fx, resolved) = indexed();
    let create_order = fx.symbol("orders_api.routes.create_order");
    let repo = repository_id("/repo/polyglot");
    let asked = ids(&[
        &create_order.id,
        &file_id(CLIENT_TS),
        &service_id("web_gateway"),
        &repo,
        &endpoint("GET", "/orders/{order_id}", "orders_api").id,
        &table("customers").id,
        &queue("order.created", "queue").id,
        &unresolved_id("render"),
        &unresolved_id("GET /x"),
        "no-such-id",
    ]);
    let by_id: HashMap<String, NodeRec> = store
        .nodes(&asked)
        .unwrap()
        .into_iter()
        .map(|n| (n.id.clone(), n))
        .collect();
    assert_eq!(by_id.len(), 9, "the unknown id is skipped");

    assert_eq!(by_id[&create_order.id], node_of(create_order));
    assert_eq!(
        by_id[&file_id(CLIENT_TS)],
        NodeRec {
            file_path: CLIENT_TS.to_string(),
            service: "web_gateway".to_string(),
            ..NodeRec::bare(
                &file_id(CLIENT_TS),
                NodeLabel::File,
                CLIENT_TS,
                CLIENT_TS,
                lang::TYPESCRIPT
            )
        }
    );
    assert_eq!(
        by_id[&service_id("web_gateway")],
        NodeRec {
            service: "web_gateway".to_string(),
            ..NodeRec::bare(
                &service_id("web_gateway"),
                NodeLabel::Service,
                "web_gateway",
                "web_gateway",
                ""
            )
        }
    );
    assert_eq!(
        by_id[&repo],
        NodeRec::bare(&repo, NodeLabel::Repository, "/repo/polyglot", "/repo/polyglot", COMMIT)
    );

    // Derived nodes come back as the resolver gave them.
    let get_order = &by_id[&endpoint("GET", "/orders/{order_id}", "orders_api").id];
    assert_eq!(
        (
            get_order.name.as_str(),
            get_order.qualified_name.as_str(),
            get_order.kind.as_str()
        ),
        ("GET /orders/{}", "/orders/{}", "GET")
    );
    assert_eq!(get_order.service, "orders_api");
    let derived_ids: Vec<String> = resolved.nodes.iter().map(|n| n.id.clone()).collect();
    let mut derived = store.nodes(&derived_ids).unwrap();
    derived.sort_by(|a, b| a.id.cmp(&b.id));
    let mut expected = resolved.nodes.clone();
    expected.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(derived, expected);
    let customers = &by_id[&table("customers").id];
    assert_eq!(
        (
            customers.label,
            customers.name.as_str(),
            customers.qualified_name.as_str(),
            customers.kind.as_str()
        ),
        (NodeLabel::DbTable, "customers", "customers", "")
    );
    let created = &by_id[&queue("order.created", "queue").id];
    assert_eq!(
        (
            created.label,
            created.name.as_str(),
            created.qualified_name.as_str(),
            created.kind.as_str()
        ),
        (NodeLabel::Queue, "order.created", "order.created", "queue")
    );
    let render = &by_id[&unresolved_id("render")];
    assert_eq!(
        (render.label, render.name.as_str(), render.kind.as_str()),
        (NodeLabel::Symbol, "render", kind::UNRESOLVED)
    );
    let ambiguous = &by_id[&unresolved_id("GET /x")];
    assert_eq!(
        (ambiguous.label, ambiguous.kind.as_str()),
        (NodeLabel::ApiEndpoint, kind::UNRESOLVED)
    );
}

#[test]
fn derived_nodes_keep_their_service_and_location() {
    let (_dir, store, _fx, mut resolved) = indexed();
    let located = NodeRec {
        service: "orders_api".to_string(),
        file_path: "orders_api/models.py".to_string(),
        start_line: 7,
        end_line: 19,
        ..table("invoices")
    };
    let topic = NodeRec {
        service: "notifier".to_string(),
        file_path: CONSUMER_TS.to_string(),
        start_line: 3,
        end_line: 4,
        ..queue("mail.sent", "topic")
    };
    let route = NodeRec {
        file_path: ROUTES_PY.to_string(),
        start_line: 13,
        end_line: 20,
        ..endpoint("DELETE", "/orders/:id", "orders_api")
    };
    resolved.nodes.extend([located.clone(), topic.clone(), route.clone()]);
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    let mut read = store
        .nodes(&[located.id.clone(), topic.id.clone(), route.id.clone()])
        .unwrap();
    read.sort_by_key(|n| n.label);
    assert_eq!(read, vec![route, located, topic]);
}

#[test]
fn nodes_by_label_lists_in_name_order_up_to_the_limit() {
    let (_dir, store, fx, _) = indexed();
    let names = |label, limit| {
        store
            .nodes_by_label(label, limit)
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        names(NodeLabel::Service, 10),
        vec!["notifier", "orders_api", "web_gateway"]
    );
    assert_eq!(names(NodeLabel::Service, 2), vec!["notifier", "orders_api"]);
    assert!(names(NodeLabel::Service, 0).is_empty());
    assert_eq!(names(NodeLabel::Repository, 10), vec!["/repo/polyglot"]);
    assert_eq!(names(NodeLabel::DbTable, 10), vec!["customers", "orders"]);
    assert_eq!(names(NodeLabel::Queue, 10), vec!["order.created"]);
    assert_eq!(
        names(NodeLabel::ApiEndpoint, 10),
        vec!["POST /orders", "GET /orders/{}", "GET /x"]
    );

    let files = store.nodes_by_label(NodeLabel::File, 100).unwrap();
    assert_eq!(files.len(), fx.files.len());
    assert!(files.windows(2).all(|w| w[0].name < w[1].name));
    assert!(files
        .iter()
        .all(|f| f.label == NodeLabel::File && f.name == f.qualified_name && f.name == f.file_path));
    assert!(files.iter().any(|f| f.kind == lang::PYTHON) && files.iter().any(|f| f.kind == lang::TYPESCRIPT));

    // Placeholders are Symbol nodes too, marked by their kind.
    let symbols = store.nodes_by_label(NodeLabel::Symbol, 1000).unwrap();
    assert_eq!(symbols.len(), fx.symbol_count() + 1);
    assert_eq!(symbols.iter().filter(|s| s.kind == kind::UNRESOLVED).count(), 1);
    assert!(symbols.windows(2).all(|w| w[0].qualified_name <= w[1].qualified_name));
    assert_eq!(
        store.nodes_by_label(NodeLabel::Symbol, 5).unwrap(),
        symbols[..5].to_vec()
    );
}

#[test]
fn callers_walk_calls_backwards_by_depth() {
    let (_dir, store, fx, _) = indexed();
    let callers = |qn: &str, depth| {
        store
            .callers(&fx.id(qn), depth)
            .unwrap()
            .into_iter()
            .map(|h| (h.name, h.score))
            .collect::<Vec<_>>()
    };

    assert_eq!(
        callers("orders_api.repo.save_order", 1),
        vec![("create_order".to_string(), 1.0)]
    );
    assert_eq!(
        callers("orders_api.repo.save_order", 2),
        vec![
            ("create_order".to_string(), 1.0),
            ("test_create_order".to_string(), 2.0)
        ]
    );
    // Three hops through static edges: handleOrderCreated -> sendEmail -> SmtpMailer.send
    assert_eq!(
        callers("notifier.src.mailer.SmtpMailer.send", 3),
        vec![("sendEmail".to_string(), 1.0), ("handleOrderCreated".to_string(), 2.0)]
    );
    // An inferred call is still a call; the edge says how sure it is.
    assert_eq!(
        callers("notifier.src.audit.AuditLog.recordDelivery", 1),
        vec![("handleOrderCreated".to_string(), 1.0)]
    );
    assert!(callers("orders_api.tests.test_routes.test_create_order", 6).is_empty());
    assert!(store.callers("no-such-id", 3).unwrap().is_empty());

    // Hits carry the caller's place, and its test flag.
    let hits = store.callers(&fx.id("orders_api.routes.create_order"), 1).unwrap();
    assert_eq!(hits.len(), 1);
    let test = fx.symbol("orders_api.tests.test_routes.test_create_order");
    assert_eq!(
        (hits[0].id.as_str(), hits[0].file_path.as_str(), hits[0].is_test),
        (test.id.as_str(), TEST_PY, true)
    );
    assert_eq!(
        (hits[0].qualified_name.as_str(), hits[0].start_line, hits[0].end_line),
        (test.qualified_name.as_str(), 4, 8)
    );

    // The callers of a placeholder are the symbols with an unresolved call to that name.
    let unresolved = store.callers(&unresolved_id("render"), 2).unwrap();
    assert_eq!(
        unresolved.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
        vec!["handleOrderCreated"]
    );
}

/// Lowest hop count to `target` over reversed CALLS edges, by plain breadth-first search.
fn bfs_callers(edges: &[EdgeRec], target: &str, depth: usize) -> BTreeMap<String, usize> {
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in edges.iter().filter(|e| e.edge_type == EdgeType::Calls) {
        reverse.entry(e.to.as_str()).or_default().push(e.from.as_str());
    }
    let mut hops: BTreeMap<String, usize> = BTreeMap::new();
    let mut queue = VecDeque::from([(target, 0usize)]);
    while let Some((node, d)) = queue.pop_front() {
        if d == depth {
            continue;
        }
        for caller in reverse.get(node).into_iter().flatten() {
            if *caller != target && !hops.contains_key(*caller) {
                hops.insert(caller.to_string(), d + 1);
                queue.push_back((caller, d + 1));
            }
        }
    }
    hops
}

#[test]
fn callers_agree_with_breadth_first_search_on_a_graph_with_cycles() {
    let (_dir, store) = open();
    let f = file("g/graph.py", "g", lang::PYTHON);
    let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
    let symbols: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| sym(&f, &format!("g.graph.{n}"), kind::FUNCTION, i as i64 + 1, i as i64 + 1))
        .collect();
    let id = |n: &str| symbols.iter().find(|s| s.name == n).unwrap().id.clone();
    store.insert_parsed(&parsed(f, symbols.clone()), COMMIT, NOW).unwrap();

    // A cycle a -> b -> c -> a, a diamond into c, a self call, a long tail and two ways to reach a.
    let pairs = [
        ("a", "b"),
        ("b", "c"),
        ("c", "a"),
        ("d", "c"),
        ("d", "b"),
        ("e", "d"),
        ("e", "e"),
        ("f", "e"),
        ("g", "f"),
        ("h", "g"),
        ("i", "h"),
        ("j", "i"),
        ("j", "a"),
    ];
    let edges: Vec<EdgeRec> = pairs
        .iter()
        .enumerate()
        .map(|(n, (from, to))| {
            let m = if n % 3 == 0 { method::INFERRED } else { method::STATIC };
            edge(
                &id(from),
                &id(to),
                EdgeType::Calls,
                0.5,
                m,
                "g/graph.py",
                n as i64,
                n as i64,
            )
        })
        .collect();
    store
        .replace_edges(
            &Resolved {
                edges: edges.clone(),
                ..Resolved::default()
            },
            COMMIT,
            NOW,
        )
        .unwrap();
    assert_eq!(store.last_skipped_edges(), 0);

    for target in names {
        for depth in 1..=6u8 {
            let got: BTreeMap<String, usize> = store
                .callers(&id(target), depth)
                .unwrap()
                .into_iter()
                .map(|h| (h.id, h.score as usize))
                .collect();
            assert_eq!(
                got,
                bfs_callers(&edges, &id(target), depth as usize),
                "callers of {target} within {depth}"
            );
        }
    }
    // Nearest first, then by name.
    let order: Vec<String> = store
        .callers(&id("c"), 6)
        .unwrap()
        .into_iter()
        .map(|h| h.name)
        .collect();
    assert_eq!(order, vec!["b", "d", "a", "e", "f", "j", "g", "h", "i"]);
}

#[test]
fn unresolved_out_counts_unresolved_edges_of_any_type() {
    let (_dir, store, fx, mut resolved) = indexed();
    let handle = fx.id("notifier.src.consumer.handleOrderCreated");
    assert_eq!(store.unresolved_out(&handle).unwrap(), 1, "the call to render");
    assert_eq!(
        store
            .unresolved_out(&fx.id("web_gateway.src.ordersClient.probe"))
            .unwrap(),
        1,
        "the HTTP call"
    );
    assert_eq!(
        store.unresolved_out(&fx.id("orders_api.routes.create_order")).unwrap(),
        0
    );
    assert_eq!(store.unresolved_out("no-such-id").unwrap(), 0);

    // An unresolved base class and an unresolved interface count as well.
    let smtp = fx.id("notifier.src.mailer.SmtpMailer");
    assert_eq!(store.unresolved_out(&smtp).unwrap(), 0);
    resolved.nodes.push(placeholder("Transport"));
    resolved.edges.push(edge(
        &smtp,
        &unresolved_id("Transport"),
        EdgeType::Inherits,
        0.0,
        method::UNRESOLVED,
        MAILER_TS,
        14,
        14,
    ));
    resolved.edges.push(edge(
        &smtp,
        &unresolved_id("render"),
        EdgeType::Implements,
        0.0,
        method::UNRESOLVED,
        MAILER_TS,
        14,
        14,
    ));
    resolved.edges.push(edge(
        &smtp,
        &unresolved_id("render"),
        EdgeType::Calls,
        0.0,
        method::UNRESOLVED,
        MAILER_TS,
        20,
        20,
    ));
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    assert_eq!(store.unresolved_out(&smtp).unwrap(), 3);
    assert_eq!(store.unresolved_out(&handle).unwrap(), 1);
}

#[test]
fn stats_keep_the_old_labels_first() {
    let (_dir, store, fx, resolved) = indexed();
    let stats = store.stats().unwrap();
    let labels: Vec<&str> = stats.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(
        labels[..12],
        [
            "files",
            "symbols",
            "call_sites",
            "calls_static",
            "calls_inferred",
            "calls_unresolved",
            "imports",
            "services",
            "endpoints",
            "tables",
            "queues",
            "fact_sites"
        ]
    );
    // After them, one line per other edge type and method that occurs, in edge type order.
    assert_eq!(
        labels[12..],
        [
            "inherits_static",
            "implements_static",
            "references_inferred",
            "exposes_api_static",
            "calls_api_inferred",
            "calls_api_unresolved",
            "reads_from_static",
            "reads_from_inferred",
            "writes_to_static",
            "publishes_static",
            "consumes_static",
            "tested_by_static",
            "depends_on_static",
            "depends_on_inferred"
        ]
    );
    assert!(stats[12..].iter().all(|(_, n)| *n > 0));

    let get: BTreeMap<&str, i64> = stats.iter().map(|(l, n)| (l.as_str(), *n)).collect();
    assert_eq!(get["files"], fx.files.len() as i64);
    assert_eq!(
        get["symbols"],
        fx.symbol_count() as i64,
        "the placeholder is not a symbol"
    );
    assert_eq!(
        get["calls_static"],
        resolved.stats.get(EdgeType::Calls, method::STATIC) as i64
    );
    assert_eq!(
        get["calls_inferred"],
        resolved.stats.get(EdgeType::Calls, method::INFERRED) as i64
    );
    assert_eq!(
        get["calls_unresolved"],
        resolved.stats.get(EdgeType::Calls, method::UNRESOLVED) as i64
    );
    assert_eq!(get["imports"], resolved.stats.total(EdgeType::Imports) as i64);
    assert_eq!(
        (get["services"], get["endpoints"], get["tables"], get["queues"]),
        (3, 2, 2, 1)
    );
    for (key, n) in &resolved.stats.counts {
        let (edge_type, how) = key.split_once('/').unwrap();
        if edge_type != "CALLS" && edge_type != "IMPORTS" {
            assert_eq!(
                get[format!("{}_{how}", edge_type.to_lowercase()).as_str()],
                *n as i64,
                "{key}"
            );
        }
    }
}

#[test]
fn raw_runs_cypher_and_prints_the_table() {
    let (_dir, store, fx, _) = indexed();
    assert_eq!(
        raw_rows(&store, "MATCH (f:File) RETURN count(f);"),
        vec![fx.files.len().to_string()]
    );
    let out = store.raw("MATCH (s:Service) RETURN s.name ORDER BY s.name;").unwrap();
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec!["s.name", "notifier", "orders_api", "web_gateway"]
    );
    assert!(store.raw("THIS IS NOT CYPHER").is_err());
}

/// The fixture with a long call chain added: enough edges for the bulk load.
fn big_fixture() -> (tempfile::TempDir, Store, Fixture, Resolved, Resolved) {
    let (dir, store) = open();
    let fx = fixture();
    let small = resolved(&fx);
    let (chain_files, chain_edges, _) = chain(3000);
    store.set_repository("/repo/polyglot", COMMIT).unwrap();
    store.insert_parsed_bulk(&fx.files, COMMIT, NOW).unwrap();
    store.insert_parsed_bulk(&chain_files, COMMIT, NOW).unwrap();
    let mut big = small.clone();
    big.edges.extend(chain_edges.edges);
    (dir, store, fx, small, big)
}

#[test]
fn the_bulk_load_stores_what_single_statements_store() {
    // The same fixture edges, written once by statements and once in bulk.
    let (_d1, by_statement, _, _) = indexed();
    let (_d2, in_bulk, _fx, small, big) = big_fixture();
    in_bulk.replace_edges(&big, COMMIT, NOW + 5).unwrap();
    assert_eq!(in_bulk.last_skipped_edges(), 0);
    assert_eq!(in_bulk.last_bulk_load_error(), None, "the bulk load itself ran");

    for t in EdgeType::ALL.into_iter().filter(|t| *t != EdgeType::Contains) {
        let fixture_edges = of_type(&small, t);
        let from: Vec<String> = fixture_edges.iter().map(|e| e.from.clone()).collect();
        let to: Vec<String> = fixture_edges.iter().map(|e| e.to.clone()).collect();
        for (ids, dir) in [(&from, Direction::Out), (&to, Direction::In)] {
            let bulk = in_bulk.edges(ids, &[t], dir).unwrap();
            assert_eq!(sorted(bulk.clone()), fixture_edges, "{t} {dir:?} against the input");
            assert_eq!(
                bulk,
                by_statement.edges(ids, &[t], dir).unwrap(),
                "{t} {dir:?} against statements"
            );
        }
        // Commit and creation time too, and nothing is null.
        for store in [&in_bulk, &by_statement] {
            let stamps = raw_rows(
                store,
                &format!("MATCH ()-[r:{t}]->() RETURN DISTINCT r.commit, r.created_at;"),
            );
            assert_eq!(stamps, vec![format!("{COMMIT}|{}", NOW + 5)], "{t}");
        }
        let nulls = raw_rows(
            &in_bulk,
            &format!(
                "MATCH ()-[r:{t}]->() WHERE r.confidence IS NULL OR r.method IS NULL OR r.evidence_file IS NULL \
                 OR r.start_line IS NULL OR r.end_line IS NULL OR r.commit IS NULL OR r.created_at IS NULL \
                 RETURN count(r);"
            ),
        );
        assert_eq!(nulls, vec!["0"], "{t}");
    }

    // All of it, chain included, equals the input.
    let from: Vec<String> = big.edges.iter().map(|e| e.from.clone()).collect();
    assert_eq!(
        sorted(in_bulk.edges(&from, &[], Direction::Out).unwrap()),
        sorted(big.edges.clone())
    );

    // The counts differ only by the chain's calls.
    let (a, b) = (by_statement.stats().unwrap(), in_bulk.stats().unwrap());
    assert_eq!(
        a.iter().map(|(l, _)| l).collect::<Vec<_>>(),
        b.iter().map(|(l, _)| l).collect::<Vec<_>>()
    );
    for ((label, x), (_, y)) in a.iter().zip(&b) {
        match label.as_str() {
            "files" | "symbols" | "call_sites" | "fact_sites" | "services" => {}
            "calls_static" => assert_eq!(*y, x + 2999),
            _ => assert_eq!(x, y, "{label}"),
        }
    }

    // And a second bulk load leaves the same graph.
    let snapshot = |s: &Store| {
        (
            s.stats().unwrap(),
            s.edges(&from, &EdgeType::ALL, Direction::Out).unwrap(),
        )
    };
    let first = snapshot(&in_bulk);
    in_bulk.replace_edges(&big, COMMIT, NOW + 5).unwrap();
    assert_eq!(snapshot(&in_bulk), first);
}

#[test]
fn the_bulk_load_keeps_awkward_values_and_skips_strays() {
    let (_dir, store, fx, _small, mut big) = big_fixture();
    let a = fx.id("orders_api.routes.create_order");
    let targets: Vec<String> = fx.get(MAILER_TS).symbols.iter().map(|s| s.id.clone()).collect();

    // Evidence paths and confidences a CSV file could get wrong, one edge each.
    let awkward_files = [
        "",
        "dir, with comma/a.py",
        "say \"hi\".py",
        "it's 'quoted'.py",
        "C:\\repo\\a.py",
        "ends with a backslash\\",
        " padded with spaces ",
        "semi;colon|pipe.py",
        "unicode/\u{00e9}\u{4e2d}\u{1f980}.py",
        "tab\tinside.py",
        "line\nbreak.py",
        "carriage\r\nreturn.py",
    ];
    let awkward_confidences = [0.0, 0.1 + 0.2, 1.0, 1e-9, 0.123_456_789_012_345_68, f64::INFINITY];
    let mut awkward = Vec::new();
    for (i, f) in awkward_files.iter().enumerate() {
        let confidence = awkward_confidences[i % awkward_confidences.len()];
        // One edge type per target, so every (from, to, type) is its own edge.
        let t = [EdgeType::References, EdgeType::Inherits, EdgeType::Implements][i / targets.len()];
        awkward.push(edge(
            &a,
            &targets[i % targets.len()],
            t,
            confidence,
            method::INFERRED,
            f,
            -3,
            i64::MAX,
        ));
    }
    assert_eq!(awkward.len(), 12);

    let strays = [
        edge(
            &a,
            "missing-symbol",
            EdgeType::Calls,
            0.5,
            method::INFERRED,
            ROUTES_PY,
            1,
            1,
        ),
        edge(
            "missing-symbol",
            &a,
            EdgeType::References,
            0.5,
            method::INFERRED,
            ROUTES_PY,
            1,
            1,
        ),
        edge(
            &a,
            &targets[0],
            EdgeType::ReadsFrom,
            0.9,
            method::STATIC,
            ROUTES_PY,
            1,
            1,
        ),
        edge(
            &file_id(ROUTES_PY),
            &a,
            EdgeType::Calls,
            0.9,
            method::STATIC,
            ROUTES_PY,
            1,
            1,
        ),
        edge(
            &file_id(ROUTES_PY),
            &a,
            EdgeType::Contains,
            1.0,
            method::STATIC,
            ROUTES_PY,
            1,
            1,
        ),
        edge(
            &service_id("orders_api"),
            &service_id("no-such-service"),
            EdgeType::DependsOn,
            0.9,
            method::STATIC,
            "",
            0,
            0,
        ),
    ];
    let expected: Vec<EdgeRec> = big.edges.iter().chain(&awkward).cloned().collect();
    big.edges.extend(awkward.clone());
    big.edges.extend(strays.iter().cloned());

    // No commit (outside git): stored as the empty string, not as null.
    store.replace_edges(&big, "", NOW).unwrap();
    assert_eq!(store.last_skipped_edges(), strays.len());
    assert_eq!(store.last_bulk_load_error(), None, "the bulk load itself ran");

    let from: Vec<String> = big.edges.iter().map(|e| e.from.clone()).collect();
    assert_eq!(
        sorted(store.edges(&from, &[], Direction::Out).unwrap()),
        sorted(expected)
    );
    let read = store
        .edges(
            std::slice::from_ref(&a),
            &[EdgeType::References, EdgeType::Inherits, EdgeType::Implements],
            Direction::Out,
        )
        .unwrap();
    for e in &awkward {
        assert!(
            read.contains(e),
            "{:?} with confidence {}",
            e.evidence_file,
            e.confidence
        );
    }
    let total = raw_rows(&store, "MATCH ()-[r:CALLS]->() RETURN count(r);");
    assert_eq!(
        raw_rows(&store, "MATCH ()-[r:CALLS]->() WHERE r.commit = '' RETURN count(r);"),
        total
    );
    assert_eq!(
        raw_rows(
            &store,
            "MATCH ()-[r:REFERENCES]->() WHERE r.evidence_file = '' RETURN count(r);"
        ),
        vec!["1"]
    );
    assert_eq!(
        store
            .edges(std::slice::from_ref(&a), &[EdgeType::Contains], Direction::In)
            .unwrap()
            .len(),
        1
    );
}
