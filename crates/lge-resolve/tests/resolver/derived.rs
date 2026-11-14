//! Derived edges: `TESTED_BY` and service-level `DEPENDS_ON`.

use crate::support::{edge, edges_from, expect, run, Repo};
use lge_core::{fact, service_id, EdgeType};

#[test]
fn tested_by_carries_the_provenance_of_the_call() {
    let mut r = Repo::new();
    r.file("svc/service.py", "svc");
    r.file("svc/helpers.py", "svc");
    r.file("svc/views.py", "svc");
    r.file("svc/pages.py", "svc");
    r.test_file("svc/tests/test_service.py", "svc");
    r.test_file("svc/tests/conftest.py", "svc");
    let create = r.function("svc/service.py", "create");
    let helper = r.function("svc/helpers.py", "normalize");
    r.function("svc/views.py", "render");
    r.function("svc/pages.py", "render");
    let fixture = r.function("svc/tests/conftest.py", "make_client");
    let test = r.function("svc/tests/test_service.py", "test_create");
    r.import("svc/tests/test_service.py", "svc.service", "create", "create");
    r.call(&test, "", "create", 5);
    r.call(&test, "", "normalize", 6);
    r.call(&test, "", "render", 7);
    r.call(&test, "", "make_client", 8);
    r.call(&create, "", "normalize", 3);
    let res = run(&r);
    let e = expect(&res, EdgeType::TestedBy, &create, &test, "static", 0.90);
    assert_eq!(
        (e.evidence_file.as_str(), e.start_line),
        ("svc/tests/test_service.py", 5)
    );
    expect(&res, EdgeType::TestedBy, &helper, &test, "inferred", 0.50);
    // Not for an unresolved call, not for a test calling test code, not for
    // production code calling production code.
    assert_eq!(res.stats.total(EdgeType::TestedBy), 2);
    assert!(edge(&res, EdgeType::TestedBy, "unresolved:render", &test).is_none());
    assert!(edge(&res, EdgeType::TestedBy, &fixture, &test).is_none());
    assert!(edge(&res, EdgeType::TestedBy, &helper, &create).is_none());
}

#[test]
fn a_service_depends_on_the_services_its_symbols_and_files_reach() {
    let mut r = Repo::new();
    r.file("billing/invoice.py", "billing");
    r.file("shared/money.py", "shared");
    r.file("shared/models.py", "shared");
    r.file("audit/log.py", "audit");
    let total = r.function("billing/invoice.py", "total");
    let invoice = r.class("billing/invoice.py", "Invoice");
    let round = r.function("shared/money.py", "round_half_up");
    let document = r.class("shared/models.py", "Document");
    let record = r.function("audit/log.py", "record_event");
    r.import("billing/invoice.py", "shared.money", "round_half_up", "round_half_up");
    r.call(&total, "", "round_half_up", 12);
    r.fact(&invoice, fact::INHERITS, "Document", "", 20, true);
    r.call(&total, "", "record_event", 14);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &total, &round, "static", 0.90);
    expect(&res, EdgeType::Inherits, &invoice, &document, "inferred", 0.50);
    expect(&res, EdgeType::Calls, &total, &record, "inferred", 0.50);
    // The static call and import outweigh the inferred base class.
    let e = expect(
        &res,
        EdgeType::DependsOn,
        &service_id("billing"),
        &service_id("shared"),
        "static",
        0.90,
    );
    // Of the two, the call has a line to show.
    assert_eq!((e.evidence_file.as_str(), e.start_line), ("billing/invoice.py", 12));
    let guess = expect(
        &res,
        EdgeType::DependsOn,
        &service_id("billing"),
        &service_id("audit"),
        "inferred",
        0.50,
    );
    assert_eq!(
        (guess.evidence_file.as_str(), guess.start_line),
        ("billing/invoice.py", 14)
    );
    assert_eq!(res.stats.total(EdgeType::DependsOn), 2, "nothing depends on billing");
}

#[test]
fn an_import_alone_is_a_dependency() {
    let mut r = Repo::new();
    r.file("web/src/app.ts", "web");
    r.file("ui/src/button.ts", "ui");
    r.import("web/src/app.ts", "../../ui/src/button", "Button", "Button");
    let res = run(&r);
    expect(
        &res,
        EdgeType::DependsOn,
        &service_id("web"),
        &service_id("ui"),
        "static",
        0.90,
    );
}

#[test]
fn calling_an_endpoint_another_service_exposes_is_an_inferred_dependency() {
    let mut r = Repo::new();
    r.file("orders_api/routes.py", "orders_api");
    r.file("web_gateway/src/client.ts", "web_gateway");
    let handler = r.function("orders_api/routes.py", "create_order");
    let own = r.function("orders_api/routes.py", "self_check");
    let client = r.function("web_gateway/src/client.ts", "createOrder");
    r.fact(&handler, fact::API_EXPOSE, "/orders", "POST", 9, true);
    r.fact(&client, fact::API_CALL, "http://orders-api/orders", "POST", 6, true);
    r.fact(&own, fact::API_CALL, "/orders", "POST", 30, true);
    let res = run(&r);
    // The route is a literal on the server, but the link is a URL match.
    let e = expect(
        &res,
        EdgeType::DependsOn,
        &service_id("web_gateway"),
        &service_id("orders_api"),
        "inferred",
        0.60,
    );
    assert_eq!(
        (e.evidence_file.as_str(), e.start_line),
        ("web_gateway/src/client.ts", 6)
    );
    assert_eq!(
        res.stats.total(EdgeType::DependsOn),
        1,
        "a service does not depend on itself"
    );
    assert_eq!(res.stats.get(EdgeType::CallsApi, "inferred"), 2);
}

#[test]
fn consuming_a_queue_another_service_publishes_is_a_dependency() {
    let mut r = Repo::new();
    r.file("orders_api/events.py", "orders_api");
    r.file("notifier/src/consumer.ts", "notifier");
    r.file("reports/jobs.py", "reports");
    let publish = r.function("orders_api/events.py", "publish_order_created");
    let dynamic = r.function("orders_api/events.py", "publish_any");
    let consume = r.function("notifier/src/consumer.ts", "handleOrderCreated");
    let job = r.function("reports/jobs.py", "on_refund");
    r.fact(&publish, fact::QUEUE_PUBLISH, "order.created", "queue", 8, true);
    r.fact(&consume, fact::QUEUE_CONSUME, "order.created", "queue", 21, true);
    r.fact(&dynamic, fact::QUEUE_PUBLISH, "order.refunded", "queue", 15, false);
    r.fact(&job, fact::QUEUE_CONSUME, "order.refunded", "queue", 4, true);
    let res = run(&r);
    let e = expect(
        &res,
        EdgeType::DependsOn,
        &service_id("notifier"),
        &service_id("orders_api"),
        "static",
        0.90,
    );
    assert_eq!(
        (e.evidence_file.as_str(), e.start_line),
        ("notifier/src/consumer.ts", 21)
    );
    // The publisher's queue name is a guess, so the dependency is one too.
    expect(
        &res,
        EdgeType::DependsOn,
        &service_id("reports"),
        &service_id("orders_api"),
        "inferred",
        0.50,
    );
    // The publisher does not depend on its consumers.
    assert!(edges_from(&res, EdgeType::DependsOn, &service_id("orders_api")).is_empty());
}

#[test]
fn reading_a_table_another_service_writes_is_a_dependency() {
    let mut r = Repo::new();
    r.file("orders_api/repo.py", "orders_api");
    r.file("orders_api/models.py", "orders_api");
    r.file("notifier/src/consumer.ts", "notifier");
    r.file("reports/jobs.py", "reports");
    let insert = r.function("orders_api/repo.py", "insert_order");
    let customer = r.class("orders_api/models.py", "Customer");
    let consume = r.function("notifier/src/consumer.ts", "handleOrderCreated");
    let job = r.function("reports/jobs.py", "count_orders");
    r.fact(&insert, fact::DB_WRITE, "orders", "", 6, true);
    r.fact(&customer, fact::TABLE_MODEL, "customers", "", 5, true);
    r.fact(&consume, fact::DB_READ, "customers", "", 11, true);
    r.fact(&job, fact::DB_READ, "orders", "", 4, false);
    let res = run(&r);
    expect(
        &res,
        EdgeType::DependsOn,
        &service_id("notifier"),
        &service_id("orders_api"),
        "static",
        0.90,
    );
    expect(
        &res,
        EdgeType::DependsOn,
        &service_id("reports"),
        &service_id("orders_api"),
        "inferred",
        0.50,
    );
    assert_eq!(
        res.stats.total(EdgeType::DependsOn),
        2,
        "a writer does not depend on its readers"
    );
}

#[test]
fn symbols_and_files_without_a_service_take_no_part() {
    let mut r = Repo::new();
    r.file("setup.py", "");
    r.file("svc/main.py", "svc");
    let build = r.function("setup.py", "build");
    let main = r.function("svc/main.py", "main");
    r.import("svc/main.py", "setup", "build", "build");
    r.import("setup.py", "svc.main", "main", "main");
    r.call(&main, "", "build", 3);
    r.call(&build, "", "main", 3);
    r.fact(&build, fact::DB_WRITE, "orders", "", 5, true);
    r.fact(&main, fact::DB_READ, "orders", "", 6, true);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &main, &build, "static", 0.90);
    expect(&res, EdgeType::Calls, &build, &main, "static", 0.90);
    assert_eq!(res.stats.total(EdgeType::DependsOn), 0);
}

#[test]
fn unresolved_edges_make_no_dependency() {
    let mut r = Repo::new();
    r.file("a/x.py", "a");
    r.file("b/x.py", "b");
    r.file("c/app.py", "c");
    r.class("a/x.py", "Base");
    r.class("b/x.py", "Base");
    r.function("a/x.py", "render");
    r.function("b/x.py", "render");
    let child = r.class("c/app.py", "Child");
    let main = r.function("c/app.py", "main");
    r.fact(&child, fact::INHERITS, "Base", "", 3, true);
    r.call(&main, "", "render", 9);
    let res = run(&r);
    assert_eq!(res.stats.get(EdgeType::Inherits, "unresolved"), 1);
    assert_eq!(res.stats.get(EdgeType::Calls, "unresolved"), 1);
    assert_eq!(res.stats.total(EdgeType::DependsOn), 0);
    assert_eq!(res.stats.total(EdgeType::TestedBy), 0);
}

#[test]
fn the_surest_edge_of_each_side_decides_a_shared_node_dependency() {
    let mut r = Repo::new();
    r.file("orders_api/events.py", "orders_api");
    r.file("billing/events.py", "billing");
    r.file("notifier/src/consumer.ts", "notifier");
    let guessed = r.function("orders_api/events.py", "publish_dynamic");
    let written = r.function("orders_api/events.py", "publish_order_created");
    let other = r.function("billing/events.py", "republish");
    let dynamic = r.function("notifier/src/consumer.ts", "subscribeAll");
    let consume = r.function("notifier/src/consumer.ts", "handleOrderCreated");
    r.fact(&guessed, fact::QUEUE_PUBLISH, "order.created", "queue", 4, false);
    r.fact(&written, fact::QUEUE_PUBLISH, "order.created", "queue", 9, true);
    r.fact(&other, fact::QUEUE_PUBLISH, "order.created", "queue", 3, false);
    r.fact(&dynamic, fact::QUEUE_CONSUME, "order.created", "queue", 5, false);
    r.fact(&consume, fact::QUEUE_CONSUME, "order.created", "queue", 21, true);
    let res = run(&r);
    // Literal consumer and literal publisher: static, shown at the consumer.
    let e = expect(
        &res,
        EdgeType::DependsOn,
        &service_id("notifier"),
        &service_id("orders_api"),
        "static",
        0.90,
    );
    assert_eq!(e.start_line, 21);
    // billing only ever guesses the queue name.
    let guess = expect(
        &res,
        EdgeType::DependsOn,
        &service_id("notifier"),
        &service_id("billing"),
        "inferred",
        0.50,
    );
    assert_eq!(
        guess.start_line, 5,
        "equal strength: the first consumer is the evidence"
    );
    assert_eq!(res.stats.total(EdgeType::DependsOn), 2);
}
