//! What holds for every input: determinism, sorted output, honest provenance.

use crate::scenarios;
use crate::support::{assert_provenance, assert_same, assert_static_is_justified, expect, node, run, shuffled, Repo};
use lge_core::{
    endpoint_id, fact, file_id, queue_id, service_id, table_id, EdgeType, NodeLabel, ResolveInput, Resolved,
};

#[test]
fn empty_input_gives_empty_output() {
    let res = lge_resolve::resolve(&ResolveInput::default());
    assert!(res.nodes.is_empty());
    assert!(res.edges.is_empty());
    assert!(res.stats.counts.is_empty());
    assert_eq!((res.stats.calls_external, res.stats.imports_external), (0, 0));
}

#[test]
fn the_same_input_in_any_order_gives_the_same_output() {
    for (name, repo) in scenarios::all() {
        let first = lge_resolve::resolve(&repo.input);
        assert!(!first.edges.is_empty(), "{name}");
        assert_same(&first, &lge_resolve::resolve(&repo.input), name);
        for seed in 0..25 {
            assert_same(&first, &lge_resolve::resolve(&shuffled(&repo.input, seed)), name);
        }
        let mut reversed = repo.input.clone();
        reversed.files.reverse();
        reversed.symbols.reverse();
        reversed.calls.reverse();
        reversed.imports.reverse();
        reversed.facts.reverse();
        assert_same(&first, &lge_resolve::resolve(&reversed), name);
    }
}

/// No scenario may contain a `static` edge that the source does not spell out:
/// static edges are in the 0.90 to 0.95 band and follow from a shared file, an
/// import or a literal fact; inferred edges are between 0.40 and 0.60;
/// unresolved edges have confidence 0 and point at a placeholder.
#[test]
fn no_scenario_stores_a_guess_as_static() {
    for (name, repo) in scenarios::all() {
        let res = lge_resolve::resolve(&repo.input);
        assert_provenance(&res);
        assert_static_is_justified(&repo.input, &res);
        let by_method = |method: &str| res.edges.iter().filter(|e| e.method == method).count();
        assert_eq!(
            by_method("static") + by_method("inferred") + by_method("unresolved"),
            res.edges.len(),
            "{name}"
        );
    }
    // The three scenarios together exercise every method.
    let all: Vec<Resolved> = scenarios::all()
        .iter()
        .map(|(_, repo)| lge_resolve::resolve(&repo.input))
        .collect();
    for method in ["static", "inferred", "unresolved"] {
        assert!(
            all.iter().any(|res| res.edges.iter().any(|e| e.method == method)),
            "{method}"
        );
    }
}

#[test]
fn output_is_sorted_by_label_and_id_and_by_type_from_to() {
    let res = run(&scenarios::polyglot());
    let node_keys: Vec<(NodeLabel, &str)> = res.nodes.iter().map(|n| (n.label, n.id.as_str())).collect();
    let mut sorted_nodes = node_keys.clone();
    sorted_nodes.sort();
    assert_eq!(node_keys, sorted_nodes);
    let edge_keys: Vec<(EdgeType, &str, &str)> = res
        .edges
        .iter()
        .map(|e| (e.edge_type, e.from.as_str(), e.to.as_str()))
        .collect();
    let mut sorted_edges = edge_keys.clone();
    sorted_edges.sort();
    sorted_edges.dedup();
    assert_eq!(edge_keys, sorted_edges);
}

#[test]
fn polyglot_services_connect_through_api_queue_and_table() {
    let repo = scenarios::polyglot();
    let res = run(&repo);
    let (orders_api, web_gateway, notifier) = (
        service_id("orders_api"),
        service_id("web_gateway"),
        service_id("notifier"),
    );

    // orders_api: routes, table writes, queue publish, a model, a test.
    let create_order = repo.id("orders_api.routes.create_order");
    let post_orders = endpoint_id("POST", "/orders");
    let get_order = endpoint_id("GET", "/orders/{order_id}");
    expect(&res, EdgeType::ExposesApi, &create_order, &post_orders, "static", 0.90);
    expect(
        &res,
        EdgeType::ExposesApi,
        &repo.id("orders_api.routes.get_order"),
        &get_order,
        "static",
        0.90,
    );
    let endpoint = node(&res, NodeLabel::ApiEndpoint, &get_order);
    assert_eq!(
        (
            endpoint.name.as_str(),
            endpoint.qualified_name.as_str(),
            endpoint.kind.as_str(),
            endpoint.service.as_str()
        ),
        ("GET /orders/{}", "/orders/{}", "GET", "orders_api")
    );
    expect(
        &res,
        EdgeType::Calls,
        &create_order,
        &repo.id("orders_api.repo.insert_order"),
        "static",
        0.90,
    );
    expect(
        &res,
        EdgeType::Calls,
        &create_order,
        &repo.id("orders_api.events.publish_order_created"),
        "static",
        0.90,
    );
    expect(
        &res,
        EdgeType::WritesTo,
        &repo.id("orders_api.repo.insert_order"),
        &table_id("orders"),
        "static",
        0.90,
    );
    expect(
        &res,
        EdgeType::ReadsFrom,
        &repo.id("orders_api.repo.find_order"),
        &table_id("orders"),
        "static",
        0.90,
    );
    let queue = queue_id("order.created");
    expect(
        &res,
        EdgeType::Publishes,
        &repo.id("orders_api.events.publish_order_created"),
        &queue,
        "static",
        0.90,
    );
    let customer = repo.id("orders_api.models.Customer");
    expect(
        &res,
        EdgeType::WritesTo,
        &customer,
        &table_id("customers"),
        "static",
        0.90,
    );
    let py_test = repo.id("orders_api.tests.test_routes.test_create_order");
    expect(&res, EdgeType::TestedBy, &create_order, &py_test, "static", 0.90);

    // web_gateway: its own routes, and HTTP calls into orders_api.
    let get_handler = repo.id("web_gateway.src.routes.getOrderHandler");
    let fetch = repo.id("web_gateway.src.ordersClient.fetchOrder");
    let create = repo.id("web_gateway.src.ordersClient.createOrder");
    expect(
        &res,
        EdgeType::ExposesApi,
        &get_handler,
        &endpoint_id("GET", "/api/orders/:id"),
        "static",
        0.90,
    );
    expect(&res, EdgeType::Calls, &get_handler, &fetch, "static", 0.90);
    expect(
        &res,
        EdgeType::Calls,
        &get_handler,
        &repo.id("web_gateway.src.views.render"),
        "static",
        0.90,
    );
    expect(&res, EdgeType::CallsApi, &create, &post_orders, "inferred", 0.60);
    expect(&res, EdgeType::CallsApi, &fetch, &get_order, "inferred", 0.50);
    assert_eq!(res.stats.get(EdgeType::CallsApi, "static"), 0);
    assert_eq!(res.stats.get(EdgeType::CallsApi, "external"), 0);

    // notifier: consumes the queue, reads the customers table, every call provenance.
    let handle = repo.id("notifier.src.consumer.handleOrderCreated");
    let smtp = repo.id("notifier.src.mailer.SmtpMailer");
    expect(&res, EdgeType::Consumes, &handle, &queue, "static", 0.90);
    expect(
        &res,
        EdgeType::ReadsFrom,
        &handle,
        &table_id("customers"),
        "static",
        0.90,
    );
    expect(
        &res,
        EdgeType::Calls,
        &handle,
        &repo.id("notifier.src.mailer.sendEmail"),
        "static",
        0.90,
    );
    expect(
        &res,
        EdgeType::Calls,
        &handle,
        &repo.id("notifier.src.audit.AuditLog.recordDelivery"),
        "inferred",
        0.40,
    );
    expect(&res, EdgeType::Calls, &handle, "unresolved:render", "unresolved", 0.0);
    expect(
        &res,
        EdgeType::Inherits,
        &smtp,
        &repo.id("notifier.src.base.BaseMailer"),
        "static",
        0.90,
    );
    expect(
        &res,
        EdgeType::Implements,
        &smtp,
        &repo.id("notifier.src.mailer.Mailer"),
        "static",
        0.95,
    );
    expect(
        &res,
        EdgeType::References,
        &repo.id("notifier.src.consumer.start"),
        &handle,
        "static",
        0.95,
    );
    let ts_test = repo.id("notifier.src.consumer.test.handles an order");
    expect(&res, EdgeType::TestedBy, &handle, &ts_test, "static", 0.90);
    expect(
        &res,
        EdgeType::Imports,
        &file_id("notifier/src/consumer.test.ts"),
        &file_id("notifier/src/consumer.ts"),
        "static",
        0.90,
    );

    // Services: the gateway by a URL match, the notifier by a literal queue and table.
    expect(&res, EdgeType::DependsOn, &web_gateway, &orders_api, "inferred", 0.60);
    expect(&res, EdgeType::DependsOn, &notifier, &orders_api, "static", 0.90);
    assert_eq!(
        res.stats.total(EdgeType::DependsOn),
        2,
        "the unresolved `render` call links no services"
    );

    assert_eq!(res.stats.get(EdgeType::Calls, "unresolved"), 1);
    assert_eq!(
        res.stats.calls_external, 4,
        "res.json, cursor.execute, axios.post, axios.get"
    );
    assert_eq!(res.stats.imports_external, 4, "fastapi, express, axios, amqplib");
    assert_eq!(
        res.stats.total(EdgeType::Inherits),
        1,
        "Customer's `Base` is outside the repository"
    );
}

#[test]
fn untidy_input_is_resolved_without_guessing() {
    let repo = scenarios::untidy();
    let res = run(&repo);
    let main = repo
        .input
        .symbols
        .iter()
        .find(|s| s.qualified_name == "svc.app.main")
        .map(|s| s.id.clone());
    let main = main.expect("svc.app.main");
    // `parse` is imported from two modules, and `impl` names two modules.
    expect(&res, EdgeType::Calls, &main, "unresolved:parse", "unresolved", 0.0);
    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 0);
    // Repeated import sites are counted once per record, but give one edge.
    assert_eq!(res.stats.get(EdgeType::Imports, "static"), 5);
    // One table for both spellings; the literal read outweighs nothing else.
    expect(&res, EdgeType::ReadsFrom, &main, &table_id("orders"), "static", 0.90);
    expect(&res, EdgeType::WritesTo, &main, &table_id("orders"), "inferred", 0.50);
    assert_eq!(res.nodes.iter().filter(|n| n.label == NodeLabel::DbTable).count(), 1);
}

#[test]
fn a_node_described_twice_is_described_the_same_way_in_any_order() {
    let mut r = Repo::new();
    r.file("b/routes.py", "b");
    r.file("a/routes.py", "a");
    let b = r.function("b/routes.py", "show");
    let a = r.function("a/routes.py", "show");
    r.fact(&b, fact::API_EXPOSE, "/items/{id}", "GET", 3, true);
    r.fact(&a, fact::API_EXPOSE, "/items/:item", "GET", 3, true);
    r.fact(&b, fact::DB_READ, "Items", "", 5, true);
    r.fact(&a, fact::DB_READ, "items", "", 5, true);
    r.fact(&b, fact::QUEUE_PUBLISH, "items", "exchange", 7, true);
    r.fact(&a, fact::QUEUE_CONSUME, "items", "queue", 7, true);
    let res = run(&r);
    assert_eq!(
        node(&res, NodeLabel::ApiEndpoint, &endpoint_id("GET", "/items/{}")).service,
        "a"
    );
    assert_eq!(node(&res, NodeLabel::DbTable, &table_id("items")).name, "items");
    assert_eq!(node(&res, NodeLabel::Queue, &queue_id("items")).kind, "queue");
    assert_eq!(res.nodes.len(), 3);
}
