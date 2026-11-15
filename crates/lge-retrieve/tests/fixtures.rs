//! Retrieval on the mixed Python and TypeScript fixture: three services joined
//! by an HTTP call and a queue.
use lge_core::{method, EdgeType, NodeLabel};
use lge_retrieve::*;
use lge_store::{GraphStore, Store};
use std::path::Path;

fn indexed() -> (tempfile::TempDir, Store) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot");
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    let report = index_repo(&store, &root, &IndexOptions::default()).unwrap();
    assert_eq!((report.indexed, report.failed.len()), (15, 0));
    (dir, store)
}

fn id(store: &Store, qualified_name: &str) -> String {
    store
        .find(qualified_name)
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("missing {qualified_name}"))
        .id
}

fn names(list: &[Reached]) -> Vec<&str> {
    list.iter().map(|r| r.node.qualified_name.as_str()).collect()
}

#[test]
fn a_named_symbol_is_the_seed_and_the_pack_follows_it_across_services() {
    let (_dir, store) = indexed();
    let pack = evidence_pack(
        &store,
        "create_order must validate the payload",
        &PackOptions::default(),
    )
    .unwrap();
    assert_eq!(
        pack.seeds[0].qualified_name, "orders_api.routes.create_order",
        "{:?}",
        pack.seeds
    );
    let text = &pack.text;
    assert!(
        text.starts_with("# Context for: create_order must validate the payload\n"),
        "{text}"
    );
    assert!(
        text.contains("## Start here\norders_api/routes.py:10-15  function orders_api.routes.create_order"),
        "{text}"
    );
    assert!(
        text.contains("def create_order(payload: dict):"),
        "the seed body is shown\n{text}"
    );
    // The conclusions come first, worked out by the graph.
    assert!(
        text.contains(
            "## In short\n- The change starts at orders_api.routes.create_order (orders_api/routes.py:10).\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "- Changing it affects 1 symbol(s); nearest, 2 steps away: web_gateway.src.ordersClient.createOrder.\n"
        ),
        "{text}"
    );
    assert!(text.contains("- Covered by 1 test(s): orders_api.tests.test_routes.test_create_order.\n- It reaches endpoint POST /orders.\n"), "{text}");
    assert!(
        text.contains("Edges without a mark were resolved from the source and need no re-checking"),
        "{text}"
    );
    assert!(text.contains("orders_api.repo.insert_order"), "what it calls\n{text}");
    assert!(
        text.contains("## Tests to run") && text.contains("orders_api.tests.test_routes.test_create_order"),
        "{text}"
    );
    // The handler is reached from another service through the endpoint it exposes,
    // and that link is a name match: the pack says so.
    assert!(text.contains("web_gateway.src.ordersClient.createOrder"), "{text}");
    assert!(
        text.contains("CALLS_API inferred 0.60  at web_gateway/src/ordersClient.ts:13"),
        "{text}"
    );
    assert!(
        pack.services.contains(&"orders_api".to_string()) && pack.services.contains(&"web_gateway".to_string()),
        "{:?}",
        pack.services
    );
    assert!(pack.files.contains(&"orders_api/routes.py".to_string()));
    assert!(pack.tokens <= pack.budget && pack.tokens == budget::count(text));
    assert!(text.trim_end().lines().last().unwrap().starts_with("commit "), "{text}");
    assert_eq!(pack.omitted, 0);

    // Same graph, same task: byte-identical text.
    let again = evidence_pack(
        &store,
        "create_order must validate the payload",
        &PackOptions::default(),
    )
    .unwrap();
    assert_eq!(again.text, pack.text);
}

#[test]
fn a_small_budget_keeps_the_seed_and_says_what_was_left_out() {
    let (_dir, store) = indexed();
    let opts = PackOptions {
        budget: 260,
        ..PackOptions::default()
    };
    let pack = evidence_pack(&store, "handleOrderCreated should retry the email", &opts).unwrap();
    assert!(pack.tokens <= 260, "{} tokens\n{}", pack.tokens, pack.text);
    assert!(
        pack.text.contains("notifier.src.consumer.handleOrderCreated"),
        "{}",
        pack.text
    );
    assert!(
        pack.omitted > 0 && pack.text.contains("more symbol(s) not shown"),
        "{}",
        pack.text
    );
    assert!(pack.text.trim_end().lines().last().unwrap().starts_with("commit "));

    let none = evidence_pack(&store, "zzzqqq kubernetes autoscaling", &PackOptions::default()).unwrap();
    assert!(
        none.seeds.is_empty() && none.text.contains("No symbol in the code graph matches"),
        "{}",
        none.text
    );
}

#[test]
fn unverified_edges_are_listed_for_the_reader_to_check() {
    let (_dir, store) = indexed();
    let pack = evidence_pack(
        &store,
        "handleOrderCreated should retry the email",
        &PackOptions::default(),
    )
    .unwrap();
    let text = &pack.text;
    assert!(text.contains("## Check before relying on these"), "{text}");
    assert!(
        text.contains("AuditLog.recordDelivery  CALLS inferred 0.40  at notifier/src/consumer.ts:22"),
        "{text}"
    );
    assert!(
        text.contains("-> render  CALLS unresolved (several candidates)  at notifier/src/consumer.ts:20"),
        "{text}"
    );
    assert!(
        text.contains("uses table customers") && text.contains("uses queue order.created"),
        "{text}"
    );
    assert_eq!((pack.unresolved_edges, pack.inferred_edges >= 1), (1, true));
}

#[test]
fn impact_crosses_a_service_boundary_through_an_endpoint() {
    let (_dir, store) = indexed();
    let found = impact(
        &store,
        &[id(&store, "orders_api.routes.create_order")],
        &WalkOptions::default(),
    )
    .unwrap();
    assert_eq!(
        names(&found.affected),
        [
            "web_gateway.src.ordersClient.createOrder",
            "web_gateway.src.routes.createOrderHandler"
        ]
    );
    assert!(
        found
            .affected
            .iter()
            .all(|r| r.method == method::INFERRED && r.confidence <= 0.60),
        "a URL match is never verified"
    );
    assert_eq!(found.inferred_paths, 2);
    assert_eq!(names(&found.tests), ["orders_api.tests.test_routes.test_create_order"]);
    assert_eq!(
        found.endpoints.iter().map(|r| r.node.name.as_str()).collect::<Vec<_>>(),
        ["POST /orders"]
    );
    assert_eq!(found.services, ["orders_api", "web_gateway"]);
    assert_eq!(
        found.files,
        [
            "orders_api/routes.py",
            "web_gateway/src/ordersClient.ts",
            "web_gateway/src/routes.ts"
        ]
    );

    // A leaf with one caller: the test that reaches it is found, nothing else.
    let leaf = impact(
        &store,
        &[id(&store, "notifier.src.audit.AuditLog.recordDelivery")],
        &WalkOptions::default(),
    )
    .unwrap();
    assert!(
        names(&leaf.affected).contains(&"notifier.src.consumer.handleOrderCreated"),
        "{:?}",
        names(&leaf.affected)
    );
    assert_eq!(leaf.services, ["notifier"]);
    // Depth bounds the walk.
    let near = impact(
        &store,
        &[id(&store, "orders_api.routes.create_order")],
        &WalkOptions {
            depth: 1,
            ..WalkOptions::default()
        },
    )
    .unwrap();
    assert!(near.affected.is_empty() && near.endpoints.len() == 1);
}

#[test]
fn a_trace_follows_the_request_through_the_queue_to_the_email() {
    let (_dir, store) = indexed();
    let opts = WalkOptions {
        depth: 6,
        ..WalkOptions::default()
    };
    let down = trace(
        &store,
        &id(&store, "web_gateway.src.ordersClient.createOrder"),
        true,
        &opts,
    )
    .unwrap()
    .unwrap();
    let path: Vec<(u8, &str)> = down.steps.iter().map(|r| (r.hops, r.node.name.as_str())).collect();
    for step in [
        (1, "POST /orders"),
        (2, "create_order"),
        (3, "publish_order_created"),
        (4, "order.created"),
        (5, "handleOrderCreated"),
        (6, "sendEmail"),
    ] {
        assert!(path.contains(&step), "{step:?} missing in {path:?}");
    }
    // The call site of each step is on its edge.
    let publish = down
        .steps
        .iter()
        .find(|r| r.node.name == "publish_order_created")
        .unwrap();
    assert_eq!(
        (publish.via.evidence_file.as_str(), publish.via.start_line),
        ("orders_api/routes.py", 14)
    );

    let up = trace(&store, &id(&store, "notifier.src.mailer.sendEmail"), false, &opts)
        .unwrap()
        .unwrap();
    assert!(
        names(&up.steps).contains(&"web_gateway.src.ordersClient.createOrder"),
        "{:?}",
        names(&up.steps)
    );
    assert!(trace(&store, "no-such-id", true, &opts).unwrap().is_none());
}

#[test]
fn dependencies_list_both_directions_with_provenance() {
    let (_dir, store) = indexed();
    let deps = dependencies(&store, &id(&store, "notifier.src.consumer.handleOrderCreated"))
        .unwrap()
        .unwrap();
    let uses: Vec<(EdgeType, &str, &str)> = deps
        .uses
        .iter()
        .map(|(e, n)| (e.edge_type, n.name.as_str(), e.method.as_str()))
        .collect();
    for want in [
        (EdgeType::Calls, "sendEmail", "static"),
        (EdgeType::Calls, "recordDelivery", "inferred"),
        (EdgeType::Calls, "render", "unresolved"),
        (EdgeType::ReadsFrom, "customers", "static"),
        (EdgeType::Consumes, "order.created", "static"),
    ] {
        assert!(uses.contains(&want), "{want:?} missing in {uses:?}");
    }
    assert!(deps
        .used_by
        .iter()
        .any(|(e, n)| e.edge_type == EdgeType::TestedBy && n.is_test));
    assert!(dependencies(&store, "no-such-id").unwrap().is_none());
}

#[test]
fn text_resolves_to_symbols_files_services_and_resources() {
    let (_dir, store) = indexed();
    let labels = |t: &str| {
        resolve_targets(&store, t)
            .unwrap()
            .into_iter()
            .map(|n| (n.label, n.name))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        labels("orders_api/routes.py"),
        [
            (NodeLabel::Symbol, "create_order".to_string()),
            (NodeLabel::Symbol, "get_order".to_string())
        ]
    );
    assert_eq!(
        labels("POST /orders"),
        [(NodeLabel::ApiEndpoint, "POST /orders".to_string())]
    );
    assert_eq!(labels("customers"), [(NodeLabel::DbTable, "customers".to_string())]);
    assert_eq!(
        labels("order.created"),
        [(NodeLabel::Queue, "order.created".to_string())]
    );
    assert_eq!(labels("notifier"), [(NodeLabel::Service, "notifier".to_string())]);
    assert_eq!(
        labels("handleOrderCreated")[0],
        (NodeLabel::Symbol, "handleOrderCreated".to_string())
    );
    assert!(labels("no such thing").is_empty() && labels("  ").is_empty());
}

#[test]
fn the_architecture_summary_names_what_connects_the_services() {
    let (_dir, store) = indexed();
    let text = architecture(&store, None).unwrap();
    for line in [
        "- web_gateway: depends on orders_api [inferred 0.60]; used by nothing",
        "- notifier: depends on orders_api; used by nothing",
        "- POST /orders  handled by orders_api.routes.create_order  called by web_gateway.src.ordersClient.createOrder [inferred 0.60]",
        "- order.created  published by orders_api.events.publish_order_created  consumed by notifier.src.consumer.handleOrderCreated",
        "- orders  written by orders_api.repo.insert_order  read by orders_api.repo.find_order",
    ] {
        assert!(text.contains(line), "{line}\n---\n{text}");
    }
    let only = architecture(&store, Some("notifier")).unwrap();
    assert!(
        only.contains("order.created") && !only.contains("POST /api/orders"),
        "{only}"
    );
}
