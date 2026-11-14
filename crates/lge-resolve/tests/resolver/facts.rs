//! Fact sites: endpoints, tables, queues, base classes and references.

use crate::support::{edge, edges_from, expect, node, run, Repo};
use lge_core::{endpoint_id, fact, kind, queue_id, table_id, EdgeType, NodeLabel};

/// A Python service with one handler per `(route, method)` given.
fn api(routes: &[(&str, &str, bool)]) -> (Repo, Vec<String>) {
    let mut r = Repo::new();
    r.file("orders_api/routes.py", "orders_api");
    r.file("gateway/client.ts", "gateway");
    let mut handlers = Vec::new();
    for (n, (route, method, literal)) in routes.iter().enumerate() {
        let handler = r.function("orders_api/routes.py", &format!("handler_{n}"));
        r.fact(&handler, fact::API_EXPOSE, route, method, 10 + n as i64, *literal);
        handlers.push(handler);
    }
    (r, handlers)
}

#[test]
fn api_expose_makes_an_endpoint_node_and_a_static_edge() {
    let (r, handlers) = api(&[("/orders/{order_id}", "get", true), ("/orders", "POST", true)]);
    let res = run(&r);
    let id = endpoint_id("GET", "/orders/{}");
    let e = expect(&res, EdgeType::ExposesApi, &handlers[0], &id, "static", 0.90);
    assert_eq!(
        (e.evidence_file.as_str(), e.start_line, e.end_line),
        ("orders_api/routes.py", 10, 10)
    );
    let n = node(&res, NodeLabel::ApiEndpoint, &id);
    assert_eq!(n.name, "GET /orders/{}");
    assert_eq!(n.qualified_name, "/orders/{}");
    assert_eq!(n.kind, "GET");
    assert_eq!(n.service, "orders_api");
    assert_eq!(n.file_path, "orders_api/routes.py");
    let post = node(&res, NodeLabel::ApiEndpoint, &endpoint_id("POST", "/orders"));
    assert_eq!((post.name.as_str(), post.kind.as_str()), ("POST /orders", "POST"));
    assert_eq!(res.nodes.len(), 2);
    assert_eq!(res.stats.get(EdgeType::ExposesApi, "static"), 2);
}

#[test]
fn a_route_that_is_not_a_literal_is_exposed_as_inferred() {
    let (r, handlers) = api(&[("${PREFIX}/orders", "GET", false), ("/health", "", true)]);
    let res = run(&r);
    expect(
        &res,
        EdgeType::ExposesApi,
        &handlers[0],
        &endpoint_id("GET", "/orders"),
        "inferred",
        0.50,
    );
    // No method known: the endpoint answers to any.
    let any = endpoint_id("ANY", "/health");
    expect(&res, EdgeType::ExposesApi, &handlers[1], &any, "static", 0.90);
    assert_eq!(node(&res, NodeLabel::ApiEndpoint, &any).name, "ANY /health");
}

#[test]
fn two_spellings_of_one_route_are_one_endpoint() {
    let (mut r, handlers) = api(&[("/orders/{order_id}", "GET", true)]);
    r.file("legacy/routes.py", "legacy");
    let legacy = r.function("legacy/routes.py", "show");
    r.fact(&legacy, fact::API_EXPOSE, "/orders/<int:id>/", "GET", 4, true);
    let res = run(&r);
    let id = endpoint_id("GET", "/orders/{}");
    expect(&res, EdgeType::ExposesApi, &handlers[0], &id, "static", 0.90);
    expect(&res, EdgeType::ExposesApi, &legacy, &id, "static", 0.90);
    assert_eq!(res.nodes.len(), 1);
    // The first handler by file path describes the node.
    assert_eq!(node(&res, NodeLabel::ApiEndpoint, &id).service, "legacy");
}

#[test]
fn api_call_is_inferred_even_when_the_url_is_a_literal() {
    let (mut r, _) = api(&[("/orders", "POST", true), ("/orders/{order_id}", "GET", true)]);
    let create = r.function("gateway/client.ts", "createOrder");
    let fetch = r.function("gateway/client.ts", "fetchOrder");
    r.fact(&create, fact::API_CALL, "http://orders-api/orders", "POST", 6, true);
    r.fact(&fetch, fact::API_CALL, "${ORDERS_URL}/orders/${id}", "GET", 10, false);
    let res = run(&r);
    let e = expect(
        &res,
        EdgeType::CallsApi,
        &create,
        &endpoint_id("POST", "/orders"),
        "inferred",
        0.60,
    );
    assert_eq!((e.evidence_file.as_str(), e.start_line), ("gateway/client.ts", 6));
    expect(
        &res,
        EdgeType::CallsApi,
        &fetch,
        &endpoint_id("GET", "/orders/{}"),
        "inferred",
        0.50,
    );
    assert_eq!(res.stats.get(EdgeType::CallsApi, "static"), 0);
    assert_eq!(res.stats.get(EdgeType::CallsApi, "external"), 0);
}

#[test]
fn a_declared_parameter_matches_a_concrete_segment_of_the_call() {
    let (mut r, _) = api(&[("/orders/:id/items", "GET", true), ("/users/{id}", "ANY", true)]);
    let f = r.function("gateway/client.ts", "f");
    let g = r.function("gateway/client.ts", "g");
    r.fact(&f, fact::API_CALL, "/orders/42/items?page=2", "GET", 3, true);
    r.fact(&g, fact::API_CALL, "https://users.internal/users/7", "", 8, true);
    let res = run(&r);
    expect(
        &res,
        EdgeType::CallsApi,
        &f,
        &endpoint_id("GET", "/orders/{}/items"),
        "inferred",
        0.60,
    );
    // Either side may leave the method open.
    expect(
        &res,
        EdgeType::CallsApi,
        &g,
        &endpoint_id("ANY", "/users/{}"),
        "inferred",
        0.60,
    );
}

#[test]
fn calls_that_fit_no_endpoint_are_counted_as_external() {
    let (mut r, _) = api(&[("/orders/{id}", "GET", true)]);
    let f = r.function("gateway/client.ts", "f");
    r.fact(&f, fact::API_CALL, "https://api.stripe.com/v1/charges", "POST", 3, true);
    r.fact(&f, fact::API_CALL, "/orders/1", "DELETE", 4, true); // wrong method
    r.fact(&f, fact::API_CALL, "/orders", "GET", 5, true); // one segment short
    r.fact(&f, fact::API_CALL, "/orders/1/items", "GET", 6, true); // one segment long
    let res = run(&r);
    assert_eq!(res.stats.get(EdgeType::CallsApi, "external"), 4);
    assert!(edges_from(&res, EdgeType::CallsApi, &f).is_empty());
    assert_eq!(
        res.stats.calls_external, 0,
        "API calls are counted apart from symbol calls"
    );
}

#[test]
fn an_exact_path_is_preferred_over_a_parameter_match() {
    let (mut r, _) = api(&[("/orders/{id}", "GET", true), ("/orders/pending", "GET", true)]);
    let pending = r.function("gateway/client.ts", "pending");
    let one = r.function("gateway/client.ts", "one");
    r.fact(&pending, fact::API_CALL, "/orders/pending", "GET", 3, true);
    r.fact(&one, fact::API_CALL, "/orders/42", "GET", 7, true);
    let res = run(&r);
    expect(
        &res,
        EdgeType::CallsApi,
        &pending,
        &endpoint_id("GET", "/orders/pending"),
        "inferred",
        0.60,
    );
    expect(
        &res,
        EdgeType::CallsApi,
        &one,
        &endpoint_id("GET", "/orders/{}"),
        "inferred",
        0.60,
    );
    assert_eq!(res.stats.get(EdgeType::CallsApi, "unresolved"), 0);
}

#[test]
fn several_fitting_endpoints_give_an_unresolved_edge_to_a_placeholder_endpoint() {
    let (mut r, _) = api(&[("/orders/pending", "GET", true), ("/orders/latest", "GET", true)]);
    let f = r.function("gateway/client.ts", "f");
    r.fact(&f, fact::API_CALL, "${BASE}/orders/${which}", "get", 3, false);
    let res = run(&r);
    let e = expect(
        &res,
        EdgeType::CallsApi,
        &f,
        "unresolved:GET /orders/{}",
        "unresolved",
        0.0,
    );
    assert_eq!((e.evidence_file.as_str(), e.start_line), ("gateway/client.ts", 3));
    let placeholder = node(&res, NodeLabel::ApiEndpoint, "unresolved:GET /orders/{}");
    assert_eq!(placeholder.kind, kind::UNRESOLVED);
    assert_eq!(placeholder.name, "GET /orders/{}");
    assert_eq!(placeholder.qualified_name, "/orders/{}");
    assert_eq!(edges_from(&res, EdgeType::CallsApi, &f).len(), 1);
    assert_eq!(
        res.stats.total(EdgeType::DependsOn),
        0,
        "an unresolved call is no service dependency"
    );
}

#[test]
fn the_same_route_under_two_methods_is_unresolved_when_the_call_has_none() {
    let (mut r, _) = api(&[("/orders", "GET", true), ("/orders", "POST", true)]);
    let f = r.function("gateway/client.ts", "f");
    r.fact(&f, fact::API_CALL, "/orders", "", 3, true);
    let res = run(&r);
    expect(
        &res,
        EdgeType::CallsApi,
        &f,
        "unresolved:ANY /orders",
        "unresolved",
        0.0,
    );
}

#[test]
fn table_reads_and_writes_point_at_one_table_node() {
    let mut r = Repo::new();
    r.file("orders_api/repo.py", "orders_api");
    r.file("gateway/db.ts", "gateway");
    let insert = r.function("orders_api/repo.py", "insert_order");
    let find = r.function("orders_api/repo.py", "find_order");
    let prisma = r.function("gateway/db.ts", "listOrders");
    r.fact(&insert, fact::DB_WRITE, "orders", "", 5, true);
    r.fact(&find, fact::DB_READ, "Orders", "", 9, true);
    r.fact(&prisma, fact::DB_READ, "orders", "", 3, false);
    r.fact(&prisma, fact::DB_WRITE, "audit_log", "", 4, false);
    let res = run(&r);
    let orders = table_id("orders");
    let w = expect(&res, EdgeType::WritesTo, &insert, &orders, "static", 0.90);
    assert_eq!((w.evidence_file.as_str(), w.start_line), ("orders_api/repo.py", 5));
    expect(&res, EdgeType::ReadsFrom, &find, &orders, "static", 0.90);
    // Prisma derives the table from a property name: not a literal, so not static.
    expect(&res, EdgeType::ReadsFrom, &prisma, &orders, "inferred", 0.50);
    expect(
        &res,
        EdgeType::WritesTo,
        &prisma,
        &table_id("audit_log"),
        "inferred",
        0.50,
    );
    let n = node(&res, NodeLabel::DbTable, &orders);
    assert_eq!(
        (n.name.as_str(), n.qualified_name.as_str(), n.kind.as_str()),
        ("orders", "orders", "")
    );
    assert_eq!(res.nodes.len(), 2, "`orders` and `Orders` are one table");
}

#[test]
fn a_table_model_reads_and_writes_its_table_and_its_users_read_it() {
    let mut r = Repo::new();
    r.file("svc/models.py", "svc");
    r.file("svc/service.py", "svc");
    r.file("svc/other.py", "svc");
    let customer = r.class("svc/models.py", "Customer");
    r.fact(&customer, fact::TABLE_MODEL, "customers", "", 5, true);
    r.import("svc/service.py", ".models", "Customer", "Customer");
    let create = r.function("svc/service.py", "create_customer");
    let query = r.function("svc/service.py", "find_customers");
    let guess = r.function("svc/other.py", "guess");
    let idle = r.function("svc/other.py", "idle");
    r.call(&create, "", "Customer", 7);
    r.fact(&query, fact::REFERENCE, "Customer", "", 12, true);
    r.call(&guess, "", "Customer", 3);
    let res = run(&r);
    let table = table_id("customers");
    expect(&res, EdgeType::ReadsFrom, &customer, &table, "static", 0.90);
    expect(&res, EdgeType::WritesTo, &customer, &table, "static", 0.90);
    // Whether a user of the model reads or writes is not known: an inferred read.
    let via_call = expect(&res, EdgeType::ReadsFrom, &create, &table, "inferred", 0.50);
    assert_eq!(
        (via_call.evidence_file.as_str(), via_call.start_line),
        ("svc/service.py", 7)
    );
    let via_reference = expect(&res, EdgeType::ReadsFrom, &query, &table, "inferred", 0.50);
    assert_eq!(via_reference.start_line, 12);
    expect(&res, EdgeType::ReadsFrom, &guess, &table, "inferred", 0.50);
    assert!(edges_from(&res, EdgeType::ReadsFrom, &idle).is_empty());
    assert!(edges_from(&res, EdgeType::WritesTo, &create).is_empty());
}

#[test]
fn a_table_model_that_is_not_a_literal_is_inferred() {
    let mut r = Repo::new();
    r.file("svc/models.py", "svc");
    let model = r.class("svc/models.py", "Dynamic");
    r.fact(&model, fact::TABLE_MODEL, "${prefix}_events", "", 5, false);
    let res = run(&r);
    let table = table_id("${prefix}_events");
    expect(&res, EdgeType::ReadsFrom, &model, &table, "inferred", 0.50);
    expect(&res, EdgeType::WritesTo, &model, &table, "inferred", 0.50);
}

#[test]
fn queues_get_a_node_with_their_kind() {
    let mut r = Repo::new();
    r.file("orders_api/events.py", "orders_api");
    r.file("notifier/src/consumer.ts", "notifier");
    let publish = r.function("orders_api/events.py", "publish_order_created");
    let fanout = r.function("orders_api/events.py", "broadcast");
    let handle = r.function("notifier/src/consumer.ts", "handleOrderCreated");
    let dynamic = r.function("notifier/src/consumer.ts", "subscribe");
    r.fact(&publish, fact::QUEUE_PUBLISH, "order.created", "queue", 8, true);
    r.fact(&fanout, fact::QUEUE_PUBLISH, "events", "exchange", 14, true);
    r.fact(&handle, fact::QUEUE_CONSUME, "order.created", "queue", 21, true);
    r.fact(&dynamic, fact::QUEUE_CONSUME, "${prefix}.updated", "", 30, false);
    let res = run(&r);
    let queue = queue_id("order.created");
    expect(&res, EdgeType::Publishes, &publish, &queue, "static", 0.90);
    let c = expect(&res, EdgeType::Consumes, &handle, &queue, "static", 0.90);
    assert_eq!(
        (c.evidence_file.as_str(), c.start_line),
        ("notifier/src/consumer.ts", 21)
    );
    expect(&res, EdgeType::Publishes, &fanout, &queue_id("events"), "static", 0.90);
    expect(
        &res,
        EdgeType::Consumes,
        &dynamic,
        &queue_id("${prefix}.updated"),
        "inferred",
        0.50,
    );
    let n = node(&res, NodeLabel::Queue, &queue);
    assert_eq!(
        (n.name.as_str(), n.qualified_name.as_str(), n.kind.as_str()),
        ("order.created", "order.created", "queue")
    );
    assert_eq!(node(&res, NodeLabel::Queue, &queue_id("events")).kind, "exchange");
    assert_eq!(
        node(&res, NodeLabel::Queue, &queue_id("${prefix}.updated")).kind,
        "queue"
    );
    assert_eq!(res.nodes.len(), 3);
}

#[test]
fn inherits_resolves_in_the_file_then_through_imports() {
    let mut r = Repo::new();
    r.file("svc/models/__init__.py", "svc");
    r.file("svc/models/base.py", "svc");
    r.file("svc/app.py", "svc");
    let base = r.class("svc/models/base.py", "Base");
    let mixin = r.class("svc/app.py", "Mixin");
    let local = r.class("svc/app.py", "Local");
    let imported = r.class("svc/app.py", "Imported");
    let dotted = r.class("svc/app.py", "Dotted");
    r.import("svc/app.py", ".models.base", "Base", "Base");
    r.import("svc/app.py", ".models", "base", "base");
    r.fact(&local, fact::INHERITS, "Mixin", "", 10, true);
    r.fact(&imported, fact::INHERITS, "Base", "", 20, true);
    r.fact(&dotted, fact::INHERITS, "base.Base", "", 30, true);
    let res = run(&r);
    let e = expect(&res, EdgeType::Inherits, &local, &mixin, "static", 0.95);
    assert_eq!((e.evidence_file.as_str(), e.start_line), ("svc/app.py", 10));
    expect(&res, EdgeType::Inherits, &imported, &base, "static", 0.90);
    expect(&res, EdgeType::Inherits, &dotted, &base, "static", 0.90);
    assert_eq!(res.stats.total(EdgeType::Inherits), 3);
}

#[test]
fn inherits_by_name_is_inferred_unresolved_or_external() {
    let mut r = Repo::new();
    r.file("a/base.py", "a");
    r.file("b/base.py", "b");
    r.file("c/app.py", "c");
    let only = r.class("a/base.py", "OnlyOne");
    r.class("a/base.py", "Twice");
    r.class("b/base.py", "Twice");
    r.function("a/base.py", "not_a_class");
    let single = r.class("c/app.py", "Single");
    let several = r.class("c/app.py", "Several");
    let external = r.class("c/app.py", "External");
    let from_function = r.class("c/app.py", "FromFunction");
    r.fact(&single, fact::INHERITS, "OnlyOne", "", 3, true);
    r.fact(&several, fact::INHERITS, "Twice", "", 9, true);
    r.fact(&external, fact::INHERITS, "Exception", "", 15, true);
    r.fact(&from_function, fact::INHERITS, "not_a_class", "", 21, true);
    let res = run(&r);
    expect(&res, EdgeType::Inherits, &single, &only, "inferred", 0.50);
    expect(
        &res,
        EdgeType::Inherits,
        &several,
        "unresolved:Twice",
        "unresolved",
        0.0,
    );
    assert_eq!(node(&res, NodeLabel::Symbol, "unresolved:Twice").kind, kind::UNRESOLVED);
    assert!(edges_from(&res, EdgeType::Inherits, &external).is_empty());
    assert!(
        edges_from(&res, EdgeType::Inherits, &from_function).is_empty(),
        "a function is not a base class"
    );
    assert_eq!(res.stats.total(EdgeType::Inherits), 2);
    assert_eq!(res.stats.calls_external, 0);
}

#[test]
fn implements_and_extends_in_typescript() {
    let mut r = Repo::new();
    r.file("notifier/src/mailer.ts", "notifier");
    r.file("notifier/src/base.ts", "notifier");
    r.file("notifier/src/types.ts", "notifier");
    let smtp = r.class("notifier/src/mailer.ts", "SmtpMailer");
    let mailer = r.def("notifier/src/mailer.ts", "Mailer", kind::INTERFACE);
    let base = r.class("notifier/src/base.ts", "BaseMailer");
    let closable = r.def("notifier/src/types.ts", "Closable", kind::INTERFACE);
    r.def("notifier/src/types.ts", "Options", kind::TYPE);
    r.fact(&base, fact::DEFAULT_EXPORT, "BaseMailer", "", 1, true);
    r.import("notifier/src/mailer.ts", "./base", "default", "Base");
    r.import("notifier/src/mailer.ts", "./types", "*", "types");
    r.fact(&smtp, fact::INHERITS, "Base<Options>", "", 9, true);
    r.fact(&smtp, fact::IMPLEMENTS, "Mailer", "", 9, true);
    r.fact(&smtp, fact::IMPLEMENTS, "types.Closable", "", 9, true);
    r.fact(&smtp, fact::IMPLEMENTS, "Options", "", 9, true);
    let res = run(&r);
    expect(&res, EdgeType::Inherits, &smtp, &base, "static", 0.90);
    expect(&res, EdgeType::Implements, &smtp, &mailer, "static", 0.95);
    expect(&res, EdgeType::Implements, &smtp, &closable, "static", 0.90);
    assert_eq!(
        res.stats.total(EdgeType::Implements),
        2,
        "a type alias is not implemented"
    );
}

#[test]
fn a_base_that_is_not_written_as_a_name_is_never_static() {
    let mut r = Repo::new();
    r.file("svc/app.py", "svc");
    let base = r.class("svc/app.py", "Base");
    let child = r.class("svc/app.py", "Child");
    r.fact(&child, fact::INHERITS, "Base", "", 8, false);
    let res = run(&r);
    expect(&res, EdgeType::Inherits, &child, &base, "inferred", 0.50);
}

#[test]
fn reference_resolves_like_a_bare_name() {
    let mut r = Repo::new();
    r.file("svc/app.py", "svc");
    r.file("svc/handlers.py", "svc");
    r.file("svc/far.py", "svc");
    let start = r.function("svc/app.py", "start");
    let local = r.function("svc/app.py", "on_local");
    let imported = r.function("svc/handlers.py", "on_message");
    let far = r.def("svc/far.py", "Settings", kind::TYPE);
    r.import("svc/app.py", ".handlers", "on_message", "on_message");
    r.fact(&start, fact::REFERENCE, "on_local", "", 4, true);
    r.fact(&start, fact::REFERENCE, "on_message", "", 5, true);
    r.fact(&start, fact::REFERENCE, "Settings", "", 6, true);
    r.fact(&start, fact::REFERENCE, "undefined_name", "", 7, true);
    let res = run(&r);
    let e = expect(&res, EdgeType::References, &start, &local, "static", 0.95);
    assert_eq!((e.evidence_file.as_str(), e.start_line), ("svc/app.py", 4));
    expect(&res, EdgeType::References, &start, &imported, "static", 0.90);
    expect(&res, EdgeType::References, &start, &far, "inferred", 0.50);
    assert_eq!(res.stats.total(EdgeType::References), 3);
}

#[test]
fn an_ambiguous_reference_is_not_stored() {
    let mut r = Repo::new();
    r.file("a/x.py", "a");
    r.file("b/x.py", "b");
    r.file("c/app.py", "c");
    r.function("a/x.py", "callback");
    r.function("b/x.py", "callback");
    let start = r.function("c/app.py", "start");
    r.fact(&start, fact::REFERENCE, "callback", "", 4, true);
    let res = run(&r);
    assert!(res.edges.is_empty());
    assert!(res.nodes.is_empty(), "no placeholder for a reference");
}

#[test]
fn a_reference_to_a_method_by_its_bare_name_is_only_a_guess() {
    let mut r = Repo::new();
    r.file("svc/app.py", "svc");
    r.class("svc/app.py", "Worker");
    let method = r.method("svc/app.py", "Worker", "process");
    let start = r.method("svc/app.py", "Worker", "start");
    let free = r.function("svc/app.py", "main");
    r.fact(&free, fact::REFERENCE, "process", "", 20, true);
    r.fact(&start, fact::REFERENCE, "self.process", "", 9, true);
    let res = run(&r);
    // A bare identifier cannot name a method, even one in the same file.
    expect(&res, EdgeType::References, &free, &method, "inferred", 0.50);
    // Written through `self`, it is the method of the enclosing class.
    expect(&res, EdgeType::References, &start, &method, "static", 0.95);
}

#[test]
fn facts_without_a_known_symbol_or_key_and_default_exports_make_no_edge() {
    let mut r = Repo::new();
    r.file("svc/app.ts", "svc");
    let f = r.function("svc/app.ts", "f");
    r.fact("not-a-symbol", fact::DB_READ, "orders", "", 3, true);
    r.fact("not-a-symbol", fact::API_EXPOSE, "/orders", "GET", 4, true);
    r.fact(&f, fact::DB_WRITE, " ", "", 5, true);
    r.fact(&f, fact::QUEUE_PUBLISH, "", "queue", 6, true);
    r.fact(&f, fact::API_CALL, "", "GET", 7, true);
    r.fact(&f, fact::DEFAULT_EXPORT, "f", "", 8, true);
    r.fact(&f, "some_future_kind", "x", "", 9, true);
    let res = run(&r);
    assert!(res.edges.is_empty(), "{:#?}", res.edges);
    assert!(res.nodes.is_empty(), "{:#?}", res.nodes);
    assert!(res.stats.counts.is_empty());
    assert!(edge(&res, EdgeType::ReadsFrom, "not-a-symbol", &table_id("orders")).is_none());
}

#[test]
fn a_fact_whose_symbol_is_not_known_belongs_to_the_module_of_its_file() {
    let mut r = Repo::new();
    let routes = r.file("web/src/routes.ts", "web");
    r.file("web/src/models.ts", "web");
    // `router.get("/orders", listOrders)` with the handler imported from another file.
    r.fact_in(
        "web/src/routes.ts",
        "handler-in-another-file",
        fact::API_EXPOSE,
        "/orders",
        "GET",
        12,
        true,
    );
    r.fact_in(
        "web/src/routes.ts",
        "",
        fact::QUEUE_PUBLISH,
        "order.viewed",
        "topic",
        14,
        true,
    );
    // A fact about one particular class cannot move to the module.
    r.fact_in(
        "web/src/models.ts",
        "missing-class",
        fact::TABLE_MODEL,
        "orders",
        "",
        3,
        true,
    );
    r.fact_in(
        "web/src/models.ts",
        "missing-class",
        fact::INHERITS,
        "Base",
        "",
        2,
        true,
    );
    // No file, no owner.
    r.fact_in("web/src/gone.ts", "missing", fact::DB_READ, "orders", "", 1, true);
    let res = run(&r);
    let endpoint = endpoint_id("GET", "/orders");
    expect(&res, EdgeType::ExposesApi, &routes, &endpoint, "static", 0.90);
    assert_eq!(node(&res, NodeLabel::ApiEndpoint, &endpoint).service, "web");
    expect(
        &res,
        EdgeType::Publishes,
        &routes,
        &queue_id("order.viewed"),
        "static",
        0.90,
    );
    assert_eq!(res.edges.len(), 2, "{:#?}", res.edges);
    assert_eq!(res.nodes.len(), 2, "{:#?}", res.nodes);
}
