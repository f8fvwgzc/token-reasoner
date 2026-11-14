//! Traversals that run inside the database: bounded reach over several edge
//! types, and the shortest path with its provenance.

mod common;

use common::*;
use lge_core::{
    file_id, kind, lang, method, repository_id, service_id, unresolved_id, Direction, EdgeRec, EdgeType, NodeLabel,
    Resolved,
};
use lge_store::{GraphStore, Store};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

/// A reach, checked for what must hold whatever is asked, as (id, hops).
fn reach(
    store: &Store,
    start: &[String],
    types: &[EdgeType],
    dir: Direction,
    depth: u8,
    limit: usize,
) -> Vec<(String, u8)> {
    let found = store.reach(start, types, dir, depth, limit).unwrap();
    assert!(found.len() <= limit);
    let pairs: Vec<(String, u8)> = found.iter().map(|(n, hops)| (n.id.clone(), *hops)).collect();
    let mut sorted = pairs.clone();
    sorted.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    assert_eq!(pairs, sorted, "nearest first, then by id");
    let distinct: BTreeSet<&String> = pairs.iter().map(|(id, _)| id).collect();
    assert_eq!(distinct.len(), pairs.len(), "a node twice");
    assert!(pairs.iter().all(|(id, _)| !start.contains(id)), "a start node");
    assert!(pairs.iter().all(|(_, hops)| (1..=6).contains(hops)));
    assert!(
        found.iter().all(|(node, _)| node.kind != kind::UNRESOLVED),
        "a placeholder"
    );
    // A node is the record a lookup by id gives (checked for the nearest ones).
    for (node, _) in found.iter().take(20) {
        assert_eq!(store.nodes(std::slice::from_ref(&node.id)).unwrap(), vec![node.clone()]);
    }
    pairs
}

/// Lowest hop count from any start node, by plain breadth-first search over
/// the given edges. Placeholder targets are passed in to be left out.
fn bfs(
    edges: &[EdgeRec],
    start: &[String],
    types: &[EdgeType],
    dir: Direction,
    depth: usize,
    placeholders: &[String],
) -> Vec<(String, u8)> {
    let mut next: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in edges {
        let followed = if types.is_empty() {
            e.edge_type != EdgeType::Contains
        } else {
            types.contains(&e.edge_type)
        };
        if followed {
            let (from, to) = match dir {
                Direction::Out => (e.from.as_str(), e.to.as_str()),
                Direction::In => (e.to.as_str(), e.from.as_str()),
            };
            next.entry(from).or_default().push(to);
        }
    }
    let mut hops: BTreeMap<&str, usize> = start.iter().map(|s| (s.as_str(), 0)).collect();
    let mut queue: VecDeque<&str> = start.iter().map(String::as_str).collect();
    while let Some(node) = queue.pop_front() {
        let d = hops[node];
        if d == depth {
            continue;
        }
        for to in next.get(node).into_iter().flatten() {
            if !hops.contains_key(to) {
                hops.insert(to, d + 1);
                queue.push_back(to);
            }
        }
    }
    let mut out: Vec<(String, u8)> = hops
        .into_iter()
        .filter(|(id, d)| *d > 0 && !placeholders.iter().any(|p| p == id))
        .map(|(id, d)| (id.to_string(), d as u8))
        .collect();
    out.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    out
}

/// (id, hops) pairs in the order `reach` gives them.
fn expect(items: &[(&str, u8)]) -> Vec<(String, u8)> {
    let mut out: Vec<(String, u8)> = items.iter().map(|(id, hops)| (id.to_string(), *hops)).collect();
    out.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    out
}

#[test]
fn reach_walks_several_edge_types_in_one_direction() {
    on_both_backends(|store| {
        let (fx, resolved) = index_fixture(store);
        let s = |qn: &str| fx.id(qn);
        let one = |id: &str| vec![id.to_string()];
        let create = s("orders_api.routes.create_order");
        let save = s("orders_api.repo.save_order");
        let load = s("orders_api.repo.load_order");
        let publish = s("orders_api.events.publish_created");
        let get = s("orders_api.routes.get_order");
        let test = s("orders_api.tests.test_routes.test_create_order");
        let handle = s("notifier.src.consumer.handleOrderCreated");
        let orders = table("orders").id;
        let created = queue("order.created", "queue").id;
        let get_endpoint = endpoint("GET", "/orders/{order_id}", "orders_api").id;
        let placeholders = vec![unresolved_id("render"), unresolved_id("GET /x")];

        // One type, one hop.
        assert_eq!(
            reach(store, &one(&create), &[EdgeType::Calls], Direction::Out, 1, 10),
            expect(&[(&save, 1), (&publish, 1)])
        );
        // Several types: the walk leaves the symbols and ends in a table and a queue.
        let types = [EdgeType::Calls, EdgeType::WritesTo, EdgeType::Publishes];
        assert_eq!(
            reach(store, &one(&create), &types, Direction::Out, 2, 10),
            expect(&[(&save, 1), (&publish, 1), (&orders, 2), (&created, 2)])
        );
        let labels: BTreeSet<NodeLabel> = store
            .reach(&one(&create), &types, Direction::Out, 2, 10)
            .unwrap()
            .iter()
            .map(|(n, _)| n.label)
            .collect();
        assert_eq!(
            labels,
            BTreeSet::from([NodeLabel::Symbol, NodeLabel::DbTable, NodeLabel::Queue])
        );
        // Through a client to the endpoint it calls.
        let fetch = s("web_gateway.src.ordersClient.fetchOrder");
        assert_eq!(
            reach(
                store,
                &one(&s("web_gateway.src.routes.getOrderHandler")),
                &[EdgeType::Calls, EdgeType::CallsApi],
                Direction::Out,
                3,
                10
            ),
            expect(&[(&fetch, 1), (&get_endpoint, 2)])
        );

        // Backwards from a table: who touches it, who calls those, who tests them.
        let types = [EdgeType::ReadsFrom, EdgeType::WritesTo, EdgeType::Calls];
        assert_eq!(
            reach(store, &one(&orders), &types, Direction::In, 3, 10),
            expect(&[(&save, 1), (&load, 1), (&create, 2), (&get, 2), (&test, 3)])
        );
        assert_eq!(
            reach(store, &one(&orders), &types, Direction::In, 1, 10),
            expect(&[(&save, 1), (&load, 1)])
        );
        // The direction is one direction: nothing leaves a table.
        assert!(reach(store, &one(&orders), &types, Direction::Out, 6, 10).is_empty());

        // Services and files are nodes like any other.
        let (notifier, gateway, api) = (
            service_id("notifier"),
            service_id("web_gateway"),
            service_id("orders_api"),
        );
        assert_eq!(
            reach(store, &one(&notifier), &[EdgeType::DependsOn], Direction::Out, 6, 10),
            expect(&[(&api, 1)])
        );
        assert_eq!(
            reach(store, &one(&api), &[EdgeType::DependsOn], Direction::In, 6, 10),
            expect(&[(&notifier, 1), (&gateway, 1)])
        );
        assert_eq!(
            reach(
                store,
                &one(&file_id(ROUTES_PY)),
                &[EdgeType::Imports],
                Direction::Out,
                6,
                10
            ),
            expect(&[(&file_id(REPO_PY), 1), (&file_id(EVENTS_PY), 1)])
        );

        // CONTAINS is followed when it is asked for: repository, services, files, symbols.
        let repository = repository_id("/repo/polyglot");
        let contained = reach(store, &one(&repository), &[EdgeType::Contains], Direction::Out, 3, 1000);
        assert_eq!(contained.iter().filter(|(_, hops)| *hops == 1).count(), 3);
        assert_eq!(contained.iter().filter(|(_, hops)| *hops == 2).count(), fx.files.len());
        assert_eq!(
            contained.iter().filter(|(_, hops)| *hops == 3).count(),
            fx.symbol_count()
        );
        assert_eq!(
            reach(store, &one(&create), &[EdgeType::Contains], Direction::In, 6, 10),
            expect(&[(&file_id(ROUTES_PY), 1), (&api, 2), (&repository, 3)])
        );
        // CONTAINS mixed with resolved edges, in one walk.
        assert_eq!(
            reach(
                store,
                &one(&file_id(EVENTS_PY)),
                &[EdgeType::Contains, EdgeType::Publishes],
                Direction::Out,
                2,
                10
            ),
            expect(&[(&s("orders_api.events"), 1), (&publish, 1), (&created, 2)])
        );

        // Placeholders are never reported: the unresolved call to `render` and
        // the HTTP call with several candidate routes lead nowhere.
        assert_eq!(
            reach(store, &one(&handle), &[EdgeType::Calls], Direction::Out, 1, 10),
            expect(&[
                (&s("notifier.src.mailer.sendEmail"), 1),
                (&s("notifier.src.audit.AuditLog.recordDelivery"), 1)
            ])
        );
        let probe = s("web_gateway.src.ordersClient.probe");
        assert!(reach(store, &one(&probe), &[], Direction::Out, 6, 10).is_empty());

        // No type given: every type except CONTAINS, like a one-hop expansion.
        for start in [&create, &handle, &orders, &api, &test] {
            for dir in [Direction::Out, Direction::In] {
                for depth in [1u8, 2, 6] {
                    assert_eq!(
                        reach(store, &one(start), &[], dir, depth, 1000),
                        bfs(&resolved.edges, &one(start), &[], dir, depth as usize, &placeholders),
                        "{start} {dir:?} {depth}"
                    );
                }
            }
        }
        assert!(reach(store, &one(&create), &[], Direction::Out, 6, 1000)
            .iter()
            .all(|(id, _)| id != &file_id(ROUTES_PY)));

        // Start nodes are not reported, also when another start node reaches
        // them or a cycle leads back to them.
        assert_eq!(
            reach(
                store,
                &[create.clone(), save.clone()],
                &[EdgeType::Calls],
                Direction::Out,
                2,
                10
            ),
            expect(&[(&publish, 1)])
        );
        assert_eq!(
            reach(
                store,
                &one(&create),
                &[EdgeType::Calls, EdgeType::TestedBy],
                Direction::Out,
                6,
                10
            ),
            expect(&[(&save, 1), (&publish, 1), (&test, 1)])
        );
        // Repeated and unknown start ids change nothing.
        assert_eq!(
            reach(
                store,
                &ids(&[&create, "no-such-id", &create, ""]),
                &[EdgeType::Calls],
                Direction::Out,
                1,
                10
            ),
            expect(&[(&save, 1), (&publish, 1)])
        );

        // The limit cuts from the far end; the depth is clamped to 1..=6.
        let all = reach(store, &one(&orders), &types, Direction::In, 3, 10);
        for limit in 0..=all.len() {
            assert_eq!(
                reach(store, &one(&orders), &types, Direction::In, 3, limit),
                all[..limit]
            );
        }
        assert_eq!(reach(store, &one(&orders), &types, Direction::In, 3, usize::MAX), all);
        assert_eq!(
            reach(store, &one(&orders), &types, Direction::In, 0, 10),
            reach(store, &one(&orders), &types, Direction::In, 1, 10)
        );
        assert_eq!(
            reach(store, &one(&orders), &types, Direction::In, 255, 10),
            reach(store, &one(&orders), &types, Direction::In, 6, 10)
        );

        // Nothing to start from.
        assert!(reach(store, &[], &[], Direction::Out, 6, 10).is_empty());
        assert!(reach(store, &ids(&["no-such-id"]), &[], Direction::In, 6, 10).is_empty());
        for text in AWKWARD {
            assert!(
                reach(store, &ids(&[text]), &[], Direction::Out, 6, 10).is_empty(),
                "{text:?}"
            );
        }
        // A placeholder can be started from: its callers are real symbols.
        assert_eq!(
            reach(
                store,
                &one(&unresolved_id("render")),
                &[EdgeType::Calls],
                Direction::In,
                1,
                10
            ),
            expect(&[(&handle, 1)])
        );
    });
}

/// Ten symbols joined by edges of three types: a cycle a -> b -> c -> a, a
/// diamond into c, a self edge, a long tail and two ways to reach a.
fn cyclic_graph(store: &Store) -> (Vec<String>, Vec<EdgeRec>) {
    let f = file("g/graph.py", "g", lang::PYTHON);
    let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
    let symbols: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| sym(&f, &format!("g.graph.{n}"), kind::FUNCTION, i as i64 + 1, i as i64 + 1))
        .collect();
    let id = |n: &str| symbols.iter().find(|s| s.name == n).unwrap().id.clone();
    store.insert_parsed(&parsed(f, symbols.clone()), COMMIT, NOW).unwrap();
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
    let types = [EdgeType::Calls, EdgeType::References, EdgeType::Inherits];
    let edges: Vec<EdgeRec> = pairs
        .iter()
        .enumerate()
        .map(|(n, (from, to))| {
            edge(
                &id(from),
                &id(to),
                types[n % 3],
                0.5,
                if n % 2 == 0 { method::INFERRED } else { method::STATIC },
                "g/graph.py",
                n as i64,
                n as i64,
            )
        })
        .collect();
    let resolved = Resolved {
        edges: edges.clone(),
        ..Resolved::default()
    };
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    (names.iter().map(|n| id(n)).collect(), edges)
}

#[test]
fn reach_agrees_with_breadth_first_search_on_a_graph_with_cycles() {
    let (_dir, store) = open();
    let (nodes, edges) = cyclic_graph(&store);
    let mut starts: Vec<Vec<String>> = nodes.iter().map(|n| vec![n.clone()]).collect();
    starts.push(vec![nodes[0].clone(), nodes[4].clone()]);
    starts.push(vec![nodes[9].clone(), nodes[2].clone(), nodes[6].clone()]);
    starts.push(nodes.clone());
    let type_sets: [&[EdgeType]; 4] = [
        &[],
        &[EdgeType::Calls],
        &[EdgeType::Calls, EdgeType::References],
        &[EdgeType::Inherits, EdgeType::References, EdgeType::Calls],
    ];
    let mut compared = 0;
    for start in &starts {
        for types in type_sets {
            for dir in [Direction::Out, Direction::In] {
                for depth in [1u8, 2, 3, 6] {
                    assert_eq!(
                        reach(&store, start, types, dir, depth, usize::MAX),
                        bfs(&edges, start, types, dir, depth as usize, &[]),
                        "from {start:?} over {types:?} {dir:?} within {depth}"
                    );
                    compared += 1;
                }
            }
        }
    }
    println!("{compared} walks compared");
    // From every node at once there is nothing left to reach.
    assert!(reach(&store, &nodes, &[], Direction::Out, 6, 100).is_empty());
}

#[test]
fn reach_takes_thousands_of_start_ids() {
    let (_dir, store) = open();
    let (files, resolved, chain) = call_chain(5000);
    store.insert_parsed_bulk(&files, COMMIT, NOW).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();

    // Every other step as a start node, with unknown ids and repeats in between.
    let mut start = Vec::new();
    for (i, id) in chain.iter().enumerate().filter(|(i, _)| i % 2 == 0) {
        start.push(id.clone());
        start.push(format!("unknown-{i}"));
        start.push(id.clone());
    }
    let started = std::time::Instant::now();
    let found = reach(&store, &start, &[EdgeType::Calls], Direction::Out, 6, usize::MAX);
    println!("reach from {} ids: {:?}", start.len(), started.elapsed());
    let odd: BTreeSet<&String> = chain
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, id)| id)
        .collect();
    assert_eq!(found.len(), 2500);
    assert!(found.iter().all(|(id, hops)| odd.contains(id) && *hops == 1));
    assert_eq!(
        reach(&store, &start, &[EdgeType::Calls], Direction::Out, 6, 7),
        found[..7]
    );
    let back = reach(&store, &start, &[EdgeType::Calls], Direction::In, 1, usize::MAX);
    assert_eq!(back.len(), 2499, "the first step has no caller");

    // A node reached from two start nodes that are looked up in different
    // chunks of ids has the lower of its two hop counts.
    let mut far_apart = vec![chain[10].clone()];
    far_apart.extend((0..2100).map(|i| format!("unknown-{i}")));
    far_apart.push(chain[12].clone());
    assert_eq!(
        reach(&store, &far_apart, &[EdgeType::Calls], Direction::Out, 3, 100),
        expect(&[(&chain[11], 1), (&chain[13], 1), (&chain[14], 2), (&chain[15], 3)])
    );
    assert_eq!(
        reach(&store, &far_apart, &[EdgeType::Calls], Direction::Out, 3, 2),
        expect(&[(&chain[11], 1), (&chain[13], 1)])
    );
}

/// The stored edges of a resolution that join two nodes, in the order `edges` gives them.
fn stored(resolved: &Resolved, from: &str, to: &str, t: EdgeType) -> EdgeRec {
    resolved
        .edges
        .iter()
        .find(|e| e.from == from && e.to == to && e.edge_type == t)
        .unwrap_or_else(|| panic!("no {t} edge {from} -> {to}"))
        .clone()
}

#[test]
fn shortest_path_gives_its_edges_in_order_with_their_provenance() {
    on_both_backends(|store| {
        let (fx, resolved) = index_fixture(store);
        let s = |qn: &str| fx.id(qn);
        let test = s("orders_api.tests.test_routes.test_create_order");
        let create = s("orders_api.routes.create_order");
        let save = s("orders_api.repo.save_order");
        let orders = table("orders").id;

        // test -> handler -> repository function -> table: two calls and a write.
        let path = store.shortest_path(&test, &orders, &[], 6).unwrap().unwrap();
        assert_eq!(
            path,
            vec![
                stored(&resolved, &test, &create, EdgeType::Calls),
                stored(&resolved, &create, &save, EdgeType::Calls),
                stored(&resolved, &save, &orders, EdgeType::WritesTo),
            ]
        );
        // Each edge starts where the one before it ended.
        assert!(path.windows(2).all(|w| w[0].to == w[1].from));
        assert_eq!(
            (
                path[2].confidence,
                path[2].method.as_str(),
                path[2].evidence_file.as_str()
            ),
            (0.90, method::STATIC, REPO_PY)
        );
        assert_eq!((path[2].start_line, path[2].end_line), (5, 6));

        // Not within fewer hops than it has.
        assert_eq!(store.shortest_path(&test, &orders, &[], 2).unwrap(), None);
        assert_eq!(store.shortest_path(&test, &orders, &[], 3).unwrap(), Some(path.clone()));
        assert_eq!(
            store.shortest_path(&test, &orders, &[], 255).unwrap(),
            Some(path.clone())
        );
        // Only along the direction of the edges.
        assert_eq!(store.shortest_path(&orders, &test, &[], 6).unwrap(), None);
        // Only over the types that were asked for.
        assert_eq!(
            store.shortest_path(&test, &orders, &[EdgeType::Calls], 6).unwrap(),
            None
        );
        assert_eq!(
            store.shortest_path(&test, &save, &[EdgeType::Calls], 6).unwrap(),
            Some(path[..2].to_vec())
        );
        assert_eq!(
            store
                .shortest_path(&test, &orders, &[EdgeType::WritesTo, EdgeType::Calls], 6)
                .unwrap(),
            Some(path.clone())
        );
        // An inferred edge is part of a path, and says so.
        let handler = s("web_gateway.src.routes.createOrderHandler");
        let post = endpoint("POST", "/orders", "orders_api").id;
        let through_client = store.shortest_path(&handler, &post, &[], 6).unwrap().unwrap();
        assert_eq!(through_client.len(), 2);
        assert!(through_client.iter().all(|e| e.method == method::INFERRED));
        assert_eq!(
            through_client.iter().map(|e| e.confidence).collect::<Vec<_>>(),
            vec![0.50, 0.60]
        );

        // No path: the node is itself, unknown, or awkward text.
        assert_eq!(store.shortest_path(&test, &test, &[], 6).unwrap(), None);
        assert_eq!(store.shortest_path(&create, &create, &[], 6).unwrap(), None);
        assert_eq!(store.shortest_path(&test, "no-such-id", &[], 6).unwrap(), None);
        assert_eq!(store.shortest_path("no-such-id", &orders, &[], 6).unwrap(), None);
        assert_eq!(store.shortest_path("", "", &[], 6).unwrap(), None);
        for text in AWKWARD {
            assert_eq!(store.shortest_path(text, &orders, &[], 6).unwrap(), None, "{text:?}");
            assert_eq!(store.shortest_path(&test, text, &[], 6).unwrap(), None, "{text:?}");
        }
        // A path may end in a placeholder: the edge that leads there is unresolved.
        let handle = s("notifier.src.consumer.handleOrderCreated");
        let unresolved = store
            .shortest_path(&handle, &unresolved_id("render"), &[], 6)
            .unwrap()
            .unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(
            (unresolved[0].method.as_str(), unresolved[0].confidence),
            (method::UNRESOLVED, 0.0)
        );

        // CONTAINS is followed when asked for, and reported like a one-hop expansion reports it.
        let repository = repository_id("/repo/polyglot");
        let down = store
            .shortest_path(&repository, &create, &[EdgeType::Contains], 6)
            .unwrap()
            .unwrap();
        let chain = [
            repository.clone(),
            service_id("orders_api"),
            file_id(ROUTES_PY),
            create.clone(),
        ];
        assert_eq!(
            down.iter().map(|e| (e.from.clone(), e.to.clone())).collect::<Vec<_>>(),
            chain
                .windows(2)
                .map(|w| (w[0].clone(), w[1].clone()))
                .collect::<Vec<_>>()
        );
        for e in &down {
            let one_hop = store
                .edges(std::slice::from_ref(&e.from), &[EdgeType::Contains], Direction::Out)
                .unwrap();
            assert!(one_hop.contains(e), "{e:?}");
            assert_eq!((e.confidence, e.method.as_str()), (1.0, method::STATIC));
        }
        // Without CONTAINS in the types there is no way down.
        assert_eq!(store.shortest_path(&repository, &create, &[], 6).unwrap(), None);
        // Structure and resolved edges in one path: file -> symbol -> queue.
        let created = queue("order.created", "queue").id;
        let mixed = store
            .shortest_path(
                &file_id(EVENTS_PY),
                &created,
                &[EdgeType::Contains, EdgeType::Publishes],
                6,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            mixed.iter().map(|e| e.edge_type).collect::<Vec<_>>(),
            vec![EdgeType::Contains, EdgeType::Publishes]
        );
    });
}

#[test]
fn shortest_path_prefers_fewer_hops_and_the_most_certain_edge() {
    let (_dir, store) = open();
    let f = file("p/paths.py", "p", lang::PYTHON);
    let names = ["a", "b", "c", "d", "x", "lonely"];
    let symbols: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| sym(&f, &format!("p.paths.{n}"), kind::FUNCTION, i as i64 + 1, i as i64 + 1))
        .collect();
    let id = |n: &str| symbols.iter().find(|s| s.name == n).unwrap().id.clone();
    store.insert_parsed(&parsed(f, symbols.clone()), COMMIT, NOW).unwrap();
    let e = |from: &str, to: &str, t: EdgeType, confidence: f64, how: &str, line: i64| {
        edge(&id(from), &id(to), t, confidence, how, "p/paths.py", line, line + 1)
    };
    let (st, inf) = (method::STATIC, method::INFERRED);
    let resolved = Resolved {
        edges: vec![
            // The long way round: a -> b -> c -> d.
            e("a", "b", EdgeType::Calls, 0.95, st, 1),
            e("b", "c", EdgeType::Calls, 0.95, st, 2),
            e("c", "d", EdgeType::Calls, 0.95, st, 3),
            // The short way: a -> x -> d, with three edges between a and x.
            e("a", "x", EdgeType::Calls, 0.40, inf, 10),
            e("a", "x", EdgeType::Calls, 0.50, inf, 11),
            e("a", "x", EdgeType::References, 0.90, st, 12),
            e("x", "d", EdgeType::Inherits, 0.50, inf, 20),
            // A way back, so there are cycles to get lost in.
            e("d", "a", EdgeType::Calls, 0.95, st, 30),
        ],
        ..Resolved::default()
    };
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);

    // Two hops beat three, although the three are the more certain ones.
    let path = store.shortest_path(&id("a"), &id("d"), &[], 6).unwrap().unwrap();
    assert_eq!(
        path,
        vec![
            // Of the edges from a to x, the most certain one.
            e("a", "x", EdgeType::References, 0.90, st, 12),
            e("x", "d", EdgeType::Inherits, 0.50, inf, 20),
        ]
    );
    // Asked for calls and inheritance only, the same two nodes are joined by
    // the better of the two calls.
    assert_eq!(
        store
            .shortest_path(&id("a"), &id("d"), &[EdgeType::Calls, EdgeType::Inherits], 6)
            .unwrap(),
        Some(vec![
            e("a", "x", EdgeType::Calls, 0.50, inf, 11),
            e("x", "d", EdgeType::Inherits, 0.50, inf, 20),
        ])
    );
    // Calls only: the long way is the only way.
    assert_eq!(
        store.shortest_path(&id("a"), &id("d"), &[EdgeType::Calls], 6).unwrap(),
        Some(vec![
            e("a", "b", EdgeType::Calls, 0.95, st, 1),
            e("b", "c", EdgeType::Calls, 0.95, st, 2),
            e("c", "d", EdgeType::Calls, 0.95, st, 3),
        ])
    );
    assert_eq!(
        store.shortest_path(&id("a"), &id("d"), &[EdgeType::Calls], 2).unwrap(),
        None
    );
    // Around the cycle and back in: d -> a -> x.
    assert_eq!(
        store
            .shortest_path(&id("d"), &id("x"), &[], 6)
            .unwrap()
            .map(|p| p.len()),
        Some(2)
    );
    // A node without edges is reached from nowhere and reaches nothing.
    assert_eq!(store.shortest_path(&id("a"), &id("lonely"), &[], 30).unwrap(), None);
    assert_eq!(store.shortest_path(&id("lonely"), &id("a"), &[], 30).unwrap(), None);
}

#[test]
fn shortest_path_is_bounded_by_its_depth() {
    let (_dir, store) = open();
    let (files, resolved, chain) = call_chain(100);
    store.insert_parsed_bulk(&files, COMMIT, NOW).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();

    for (hops, max_depth, found) in [
        (1usize, 0u8, true),
        (2, 0, false),
        (1, 1, true),
        (5, 4, false),
        (5, 5, true),
        (29, 30, true),
        (30, 30, true),
        (31, 30, false),
        // The database plans no recursive match beyond 30 hops: a larger depth is 30.
        (30, 255, true),
        (31, 255, false),
        (99, 255, false),
    ] {
        let path = store
            .shortest_path(&chain[0], &chain[hops], &[EdgeType::Calls], max_depth)
            .unwrap();
        assert_eq!(path.is_some(), found, "{hops} hops within {max_depth}");
        if let Some(path) = path {
            assert_eq!(path, resolved.edges[..hops], "{hops} hops within {max_depth}");
        }
    }
    // From the middle of the chain, and never against its direction.
    assert_eq!(
        store.shortest_path(&chain[40], &chain[43], &[], 6).unwrap(),
        Some(resolved.edges[40..43].to_vec())
    );
    assert_eq!(store.shortest_path(&chain[43], &chain[40], &[], 30).unwrap(), None);
}

#[test]
fn traversals_read_the_same_after_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let fx = fixture();
    let test = fx.id("orders_api.tests.test_routes.test_create_order");
    let orders = table("orders").id;
    let handle = fx.id("notifier.src.consumer.handleOrderCreated");
    let answers = |store: &Store| {
        (
            store
                .reach(std::slice::from_ref(&test), &[], Direction::Out, 6, 100)
                .unwrap(),
            store
                .reach(std::slice::from_ref(&orders), &[], Direction::In, 6, 100)
                .unwrap(),
            store.shortest_path(&test, &orders, &[], 6).unwrap(),
            store.symbol_graph().unwrap(),
            store
                .facts_of(std::slice::from_ref(&handle), lge_core::fact::DB_READ)
                .unwrap(),
        )
    };
    let before = {
        let store = Store::open(&path).unwrap();
        index_fixture(&store);
        answers(&store)
    };
    assert_eq!(before.0.len(), 6, "{:?}", before.0);
    assert_eq!(before.2.as_ref().map(Vec::len), Some(3));
    assert_eq!(before.4.len(), 1);

    let store = Store::open(&path).unwrap();
    assert_eq!(answers(&store), before);
    // And in a process that cannot load the fts extension: a traversal does not need it.
    drop(store);
    let store = Store::open_without_fts(&path).unwrap();
    assert_eq!(answers(&store), before);
}
