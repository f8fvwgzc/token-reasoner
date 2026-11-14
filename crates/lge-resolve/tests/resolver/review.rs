//! Inputs chosen to break the resolver: guesses that could pass for static
//! edges, ambiguity hidden behind a shared node, paths that leave the root, and
//! the exact shape of the original Python fixture.

use crate::scenarios;
use crate::support::{edge, edges_from, expect, node, run, Repo};
use lge_core::{endpoint_id, fact, file_id, kind, queue_id, service_id, table_id, EdgeType, NodeLabel};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// The original fixture, edge by edge
// ---------------------------------------------------------------------------

#[test]
fn original_fixture_has_exactly_the_original_edges() {
    let repo = scenarios::original_fixture();
    let res = run(&repo);
    let name_of = |id: &str| {
        repo.input
            .symbols
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.qualified_name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let calls: BTreeSet<(String, String, String)> = res
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Calls)
        .map(|e| {
            assert_eq!(e.method, "static", "{e:?}");
            (name_of(&e.from), name_of(&e.to), format!("{:.2}", e.confidence))
        })
        .collect();
    let service = "work_svc.service.TaskService";
    let repository = "work_svc.repository.TaskRepository";
    let wanted: BTreeSet<(String, String, String)> = [
        ("auth_svc.tokens.verify_token", "auth_svc.tokens.decode", "0.95"),
        (
            "work_svc.service.TaskService.create_task",
            "work_svc.service.TaskService.validate_title",
            "0.95",
        ),
        (
            "work_svc.service.TaskService.create_task",
            "shared.util.slugify",
            "0.90",
        ),
        (
            "work_svc.service.TaskService.create_task",
            "work_svc.repository.TaskRepository.save_task",
            "0.90",
        ),
        ("work_svc.service.TaskService.create_task", repository, "0.90"),
        (
            "work_svc.service.TaskService.list_tasks",
            "work_svc.repository.TaskRepository.find_all",
            "0.90",
        ),
        ("work_svc.service.TaskService.list_tasks", repository, "0.90"),
        (
            "work_svc.controller.create_task_handler",
            "auth_svc.tokens.verify_token",
            "0.90",
        ),
        (
            "work_svc.controller.create_task_handler",
            "work_svc.service.TaskService.create_task",
            "0.90",
        ),
        ("work_svc.controller.create_task_handler", service, "0.90"),
        (
            "work_svc.controller.list_tasks_handler",
            "work_svc.service.TaskService.list_tasks",
            "0.90",
        ),
        ("work_svc.controller.list_tasks_handler", service, "0.90"),
        (
            "work_svc.tests.test_service.test_create_task",
            "work_svc.service.TaskService.create_task",
            "0.90",
        ),
        ("work_svc.tests.test_service.test_create_task", service, "0.90"),
    ]
    .into_iter()
    .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
    .collect();
    assert_eq!(calls, wanted);

    let imports: BTreeSet<(String, String)> = res
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Imports)
        .map(|e| (e.from.clone(), e.to.clone()))
        .collect();
    let wanted: BTreeSet<(String, String)> = [
        ("work_svc/service.py", "work_svc/repository.py"),
        ("work_svc/service.py", "shared/util.py"),
        ("work_svc/controller.py", "work_svc/service.py"),
        ("work_svc/controller.py", "auth_svc/tokens.py"),
        ("work_svc/tests/test_service.py", "work_svc/service.py"),
    ]
    .into_iter()
    .map(|(a, b)| (file_id(a), file_id(b)))
    .collect();
    assert_eq!(imports, wanted);
    // text.lower(), .replace() and ValueError.
    assert_eq!(res.stats.calls_external, 3);
}

// ---------------------------------------------------------------------------
// A guess is never static
// ---------------------------------------------------------------------------

#[test]
fn a_name_the_import_target_lacks_is_only_a_name_match() {
    let mut r = Repo::new();
    r.file("svc/app.py", "svc");
    r.file("svc/b.py", "svc");
    r.file("svc/c.py", "svc");
    let main = r.function("svc/app.py", "main");
    r.class("svc/b.py", "Repo");
    let helper = r.function("svc/c.py", "helper");
    r.class("svc/c.py", "Store");
    let save = r.method("svc/c.py", "Store", "save_all");
    r.import("svc/app.py", ".b", "helper", "helper");
    r.import("svc/app.py", ".b", "Repo", "Repo");
    r.call(&main, "", "helper", 3);
    r.call(&main, "Repo()", "save_all", 4);
    let res = run(&r);
    // `b` defines neither: the only symbols of that name are found by name alone.
    expect(&res, EdgeType::Calls, &main, &helper, "inferred", 0.50);
    expect(&res, EdgeType::Calls, &main, &save, "inferred", 0.40);
    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 0);
}

#[test]
fn self_never_reaches_a_class_of_the_same_name_in_another_file() {
    let mut r = Repo::new();
    // Both files have the module name `pkg.a`, so both classes are `pkg.a.A`.
    r.file("pkg/a.py", "pkg");
    r.file("pkg/a/__init__.py", "pkg");
    r.class("pkg/a.py", "A");
    let go = r.method("pkg/a.py", "A", "go");
    r.class("pkg/a/__init__.py", "A");
    let other = r.method("pkg/a/__init__.py", "A", "helper");
    r.call(&go, "self", "helper", 4);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &go, &other, "inferred", 0.40);
}

#[test]
fn facts_that_are_not_literal_are_never_static() {
    let mut r = Repo::new();
    r.file("svc/app.ts", "svc");
    let base = r.class("svc/app.ts", "Base");
    let iface = r.def("svc/app.ts", "Shape", kind::INTERFACE);
    let child = r.class("svc/app.ts", "Child");
    let callback = r.function("svc/app.ts", "callback");
    let f = r.function("svc/app.ts", "f");
    r.fact(&child, fact::INHERITS, "Base", "", 3, false);
    r.fact(&child, fact::IMPLEMENTS, "Shape", "", 3, false);
    r.fact(&f, fact::REFERENCE, "callback", "", 9, false);
    r.fact(&f, fact::API_EXPOSE, "/x", "GET", 10, false);
    r.fact(&f, fact::DB_READ, "t", "", 11, false);
    r.fact(&f, fact::DB_WRITE, "t", "", 12, false);
    r.fact(&f, fact::QUEUE_PUBLISH, "q", "queue", 13, false);
    r.fact(&f, fact::QUEUE_CONSUME, "q", "queue", 14, false);
    r.fact(&child, fact::TABLE_MODEL, "t2", "", 15, false);
    let res = run(&r);
    expect(&res, EdgeType::Inherits, &child, &base, "inferred", 0.50);
    expect(&res, EdgeType::Implements, &child, &iface, "inferred", 0.50);
    expect(&res, EdgeType::References, &f, &callback, "inferred", 0.50);
    assert!(
        res.edges.iter().all(|e| e.method == "inferred" && e.confidence == 0.50),
        "{:#?}",
        res.edges
    );
    assert_eq!(res.edges.len(), 10);
}

#[test]
fn a_default_export_the_resolver_cannot_name_blocks_the_alias_fallback() {
    let mut r = Repo::new();
    r.file("web/app.ts", "web");
    r.file("web/config.ts", "web");
    let main = r.function("web/app.ts", "main");
    let load = r.function("web/config.ts", "load");
    // `export default () => { ... }`: the indexer marked a default export, but
    // on something that is not a symbol of the input.
    r.fact_in(
        "web/config.ts",
        "anonymous-default",
        fact::DEFAULT_EXPORT,
        "default",
        "",
        12,
        true,
    );
    r.import("web/app.ts", "./config", "default", "load");
    r.call(&main, "", "load", 4);
    let res = run(&r);
    // `load()` calls the anonymous default export, not the function `load`.
    expect(&res, EdgeType::Calls, &main, &load, "inferred", 0.50);
    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 0);
}

// ---------------------------------------------------------------------------
// TypeScript and JavaScript imports
// ---------------------------------------------------------------------------

#[test]
fn a_directory_specifier_names_the_index_file_not_a_sibling_file() {
    let mut r = Repo::new();
    for path in [
        "web/src.ts",
        "web/src/index.ts",
        "web/src/app.ts",
        "web/src/lib.ts",
        "web/src/lib/index.js",
        "web.ts",
        "web/index.ts",
        "web/src/only.ts",
    ] {
        r.file(path, "web");
    }
    r.import("web/src/app.ts", ".", "*", "here");
    r.import("web/src/app.ts", "./", "*", "hereSlash");
    r.import("web/src/app.ts", "./lib/", "*", "lib");
    r.import("web/src/app.ts", "..", "*", "up");
    r.import("web/src/app.ts", "../", "*", "upSlash");
    r.import("web/src/app.ts", "./lib", "*", "libFile");
    // A directory that does not exist is not the file of that name.
    r.import("web/src/app.ts", "./only/", "*", "only");
    let res = run(&r);
    let found: BTreeSet<String> = edges_from(&res, EdgeType::Imports, &file_id("web/src/app.ts"))
        .iter()
        .map(|e| e.to.clone())
        .collect();
    let wanted: BTreeSet<String> = [
        "web/src/index.ts",
        "web/src/lib/index.js",
        "web/index.ts",
        "web/src/lib.ts",
    ]
    .into_iter()
    .map(file_id)
    .collect();
    assert_eq!(found, wanted);
    assert_eq!(res.stats.imports_external, 1);
}

#[test]
fn extensions_and_index_files_are_probed_in_the_contract_order() {
    let order = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];
    for first in 0..order.len() {
        // Files `m.<ext>` and `d/index.<ext>` for every extension from `first` on.
        let mut r = Repo::new();
        r.file("p/app.ts", "p");
        for ext in &order[first..] {
            r.file(&format!("p/m{ext}"), "p");
            r.file(&format!("p/d/index{ext}"), "p");
        }
        r.import("p/app.ts", "./m", "*", "m");
        r.import("p/app.ts", "./d", "*", "d");
        let res = run(&r);
        let found: BTreeSet<String> = edges_from(&res, EdgeType::Imports, &file_id("p/app.ts"))
            .iter()
            .map(|e| e.to.clone())
            .collect();
        let wanted: BTreeSet<String> = [
            file_id(&format!("p/m{}", order[first])),
            file_id(&format!("p/d/index{}", order[first])),
        ]
        .into_iter()
        .collect();
        assert_eq!(found, wanted, "first extension present: {}", order[first]);
    }
}

#[test]
fn import_cycles_terminate() {
    let mut r = Repo::new();
    for path in ["c/a.ts", "c/b.ts", "c/self.ts", "c/app.ts", "c/star1.ts", "c/star2.ts"] {
        r.file(path, "c");
    }
    // a and b re-export `x` from each other; nobody defines it.
    r.import("c/a.ts", "./b", "x", "x");
    r.import("c/b.ts", "./a", "x", "x");
    // Two barrels that star-export each other, and one that star-exports itself.
    r.import("c/star1.ts", "./star2", "*", "*");
    r.import("c/star2.ts", "./star1", "*", "");
    r.import("c/self.ts", "./self", "*", "*");
    r.import("c/self.ts", "./self", "y", "y");
    let real = r.function("c/star2.ts", "real");
    let main = r.function("c/app.ts", "main");
    r.import("c/app.ts", "./a", "x", "x");
    r.import("c/app.ts", "./star1", "real", "real");
    r.import("c/app.ts", "./star1", "ghost", "ghost");
    r.import("c/app.ts", "./self", "y", "y");
    r.import("c/app.ts", "./a", "default", "dflt");
    r.call(&main, "", "x", 3);
    r.call(&main, "", "real", 4);
    r.call(&main, "", "ghost", 5);
    r.call(&main, "", "y", 6);
    r.call(&main, "", "dflt", 7);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &main, &real, "static", 0.90);
    assert_eq!(res.stats.total(EdgeType::Calls), 1);
    assert_eq!(res.stats.calls_external, 4);
    // A file that imports itself has no IMPORTS edge.
    assert!(edges_from(&res, EdgeType::Imports, &file_id("c/self.ts")).is_empty());
    expect(
        &res,
        EdgeType::Imports,
        &file_id("c/a.ts"),
        &file_id("c/b.ts"),
        "static",
        0.90,
    );
    expect(
        &res,
        EdgeType::Imports,
        &file_id("c/b.ts"),
        &file_id("c/a.ts"),
        "static",
        0.90,
    );
}

#[test]
fn python_import_cycles_terminate_and_both_directions_resolve() {
    let mut r = Repo::new();
    r.file("p/a.py", "p");
    r.file("p/b.py", "p");
    let fa = r.function("p/a.py", "fa");
    let fb = r.function("p/b.py", "fb");
    r.import("p/a.py", ".b", "fb", "fb");
    r.import("p/b.py", ".a", "fa", "fa");
    r.call(&fa, "", "fb", 3);
    r.call(&fb, "", "fa", 3);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &fa, &fb, "static", 0.90);
    expect(&res, EdgeType::Calls, &fb, &fa, "static", 0.90);
    assert_eq!(res.stats.get(EdgeType::Imports, "static"), 2);
}

#[test]
fn a_python_relative_import_above_the_root_is_external() {
    let mut r = Repo::new();
    r.file("pkg/a.py", "pkg");
    r.file("x.py", "root");
    let main = r.function("pkg/a.py", "main");
    r.function("x.py", "f");
    // Three dots from `pkg/a.py` is the parent of the indexed root.
    r.import("pkg/a.py", "...x", "f", "f");
    r.import("pkg/a.py", "...", "x", "x");
    r.call(&main, "", "f", 3);
    let res = run(&r);
    assert_eq!(res.stats.total(EdgeType::Imports), 0, "{:#?}", res.edges);
    assert_eq!(res.stats.imports_external, 2);
    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 0);
}

#[test]
fn a_class_merged_with_an_interface_keeps_its_methods() {
    let mut r = Repo::new();
    r.file("svc/box.ts", "svc");
    r.file("svc/app.ts", "svc");
    // `interface Box { ... }` and `class Box { ... }` in one file: one name, two declarations.
    r.def("svc/box.ts", "Box", kind::INTERFACE);
    r.class("svc/box.ts", "Box");
    let open = r.method("svc/box.ts", "Box", "unwrap");
    let peek = r.method("svc/box.ts", "Box", "peek");
    let main = r.function("svc/app.ts", "main");
    r.import("svc/app.ts", "./box", "Box", "Box");
    r.call(&main, "Box()", "unwrap", 3);
    r.call(&open, "self", "peek", 9);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &main, &open, "static", 0.90);
    expect(&res, EdgeType::Calls, &open, &peek, "static", 0.95);
}

// ---------------------------------------------------------------------------
// Endpoint matching
// ---------------------------------------------------------------------------

/// Two services with one handler each, and a client in a third.
fn two_apis(a: (&str, &str), b: (&str, &str)) -> (Repo, String) {
    let mut r = Repo::new();
    r.file("a_api/routes.py", "a_api");
    r.file("b_api/routes.py", "b_api");
    r.file("web/client.ts", "web");
    let ha = r.function("a_api/routes.py", "handler");
    let hb = r.function("b_api/routes.py", "handler");
    r.fact(&ha, fact::API_EXPOSE, a.0, a.1, 5, true);
    r.fact(&hb, fact::API_EXPOSE, b.0, b.1, 5, true);
    let client = r.function("web/client.ts", "call");
    (r, client)
}

#[test]
fn a_route_two_services_expose_leaves_the_call_unresolved() {
    let (mut r, client) = two_apis(("/health", "GET"), ("/health/", "get"));
    r.fact(&client, fact::API_CALL, "http://a-api/health", "GET", 3, true);
    let res = run(&r);
    // One endpoint node, two services behind it: the URL path cannot say which.
    let e = expect(
        &res,
        EdgeType::CallsApi,
        &client,
        "unresolved:GET /health",
        "unresolved",
        0.0,
    );
    assert_eq!((e.evidence_file.as_str(), e.start_line), ("web/client.ts", 3));
    assert_eq!(
        node(&res, NodeLabel::ApiEndpoint, "unresolved:GET /health").kind,
        kind::UNRESOLVED
    );
    assert_eq!(edges_from(&res, EdgeType::CallsApi, &client).len(), 1);
    assert_eq!(res.stats.get(EdgeType::ExposesApi, "static"), 2);
    assert_eq!(
        res.stats.total(EdgeType::DependsOn),
        0,
        "the client depends on neither service for sure"
    );
}

#[test]
fn a_route_two_handlers_of_one_service_expose_is_still_one_target() {
    let mut r = Repo::new();
    r.file("api/v1.py", "api");
    r.file("api/v2.py", "api");
    r.file("web/client.ts", "web");
    let v1 = r.function("api/v1.py", "show");
    let v2 = r.function("api/v2.py", "show");
    r.fact(&v1, fact::API_EXPOSE, "/items/{id}", "GET", 5, true);
    r.fact(&v2, fact::API_EXPOSE, "/items/:id", "GET", 5, true);
    let client = r.function("web/client.ts", "call");
    r.fact(&client, fact::API_CALL, "/items/7", "GET", 3, true);
    let res = run(&r);
    expect(
        &res,
        EdgeType::CallsApi,
        &client,
        &endpoint_id("GET", "/items/{}"),
        "inferred",
        0.60,
    );
    expect(
        &res,
        EdgeType::DependsOn,
        &service_id("web"),
        &service_id("api"),
        "inferred",
        0.60,
    );
}

#[test]
fn any_matches_every_method_and_two_methods_are_two_candidates() {
    let (mut r, client) = two_apis(("/orders", "ANY"), ("/payments", "POST"));
    r.fact(&client, fact::API_CALL, "/orders", "DELETE", 3, true);
    r.fact(&client, fact::API_CALL, "/payments", "", 4, true);
    r.fact(&client, fact::API_CALL, "/payments", "GET", 5, true);
    let res = run(&r);
    expect(
        &res,
        EdgeType::CallsApi,
        &client,
        &endpoint_id("ANY", "/orders"),
        "inferred",
        0.60,
    );
    expect(
        &res,
        EdgeType::CallsApi,
        &client,
        &endpoint_id("POST", "/payments"),
        "inferred",
        0.60,
    );
    assert_eq!(res.stats.get(EdgeType::CallsApi, "external"), 1, "GET /payments");

    // The same route under ANY and under GET: a GET call fits both endpoints.
    let (mut r, client) = two_apis(("/orders", "ANY"), ("/orders", "GET"));
    r.fact(&client, fact::API_CALL, "/orders", "GET", 3, true);
    let res = run(&r);
    expect(
        &res,
        EdgeType::CallsApi,
        &client,
        "unresolved:GET /orders",
        "unresolved",
        0.0,
    );
    assert_eq!(res.stats.total(EdgeType::DependsOn), 0);
}

#[test]
fn a_literal_segment_never_matches_another_literal_segment() {
    let (mut r, client) = two_apis(("/orders/pending/", "GET"), ("/orders/{id}/items", "GET"));
    r.fact(&client, fact::API_CALL, "/orders/42", "GET", 3, true);
    r.fact(&client, fact::API_CALL, "/orders/pending/", "GET", 4, true);
    r.fact(&client, fact::API_CALL, "/orders/pending?x=1#top", "GET", 5, true);
    r.fact(&client, fact::API_CALL, "/orders/7/items/", "GET", 6, true);
    r.fact(&client, fact::API_CALL, "/Orders/pending", "GET", 7, true);
    let res = run(&r);
    let pending = endpoint_id("GET", "/orders/pending");
    let items = endpoint_id("GET", "/orders/{}/items");
    let found: BTreeSet<&str> = edges_from(&res, EdgeType::CallsApi, &client)
        .iter()
        .map(|e| e.to.as_str())
        .collect();
    assert_eq!(found, BTreeSet::from([pending.as_str(), items.as_str()]));
    // `/orders/42` and `/Orders/pending` fit nothing.
    assert_eq!(res.stats.get(EdgeType::CallsApi, "external"), 2);
}

#[test]
fn a_url_without_a_literal_path_segment_names_no_endpoint() {
    let (mut r, client) = two_apis(("/", "GET"), ("/health", "GET"));
    r.file("c_api/routes.py", "c_api");
    let tenant = r.function("c_api/routes.py", "tenant_home");
    r.fact(&tenant, fact::API_EXPOSE, "/{tenant}/home/{page}", "GET", 9, true);
    // A host and nothing else, a variable, a base and a variable path.
    r.fact(&client, fact::API_CALL, "https://api.example.com", "GET", 3, true);
    r.fact(&client, fact::API_CALL, "${url}", "GET", 4, false);
    r.fact(&client, fact::API_CALL, "${BASE}/${path}", "GET", 5, false);
    r.fact(&client, fact::API_CALL, "/", "GET", 6, true);
    r.fact(&client, fact::API_CALL, "${a}/${b}/${c}/${d}", "GET", 7, false);
    let res = run(&r);
    assert!(
        edges_from(&res, EdgeType::CallsApi, &client).is_empty(),
        "{:#?}",
        res.edges
    );
    assert_eq!(res.stats.get(EdgeType::CallsApi, "external"), 5);
    assert_eq!(res.stats.total(EdgeType::DependsOn), 0);

    // One written segment that agrees is enough.
    let (mut r, client) = two_apis(("/", "GET"), ("/health", "GET"));
    r.fact(&client, fact::API_CALL, "${BASE}/health", "GET", 3, false);
    let res = run(&r);
    expect(
        &res,
        EdgeType::CallsApi,
        &client,
        &endpoint_id("GET", "/health"),
        "inferred",
        0.50,
    );
}

#[test]
fn the_exposing_service_never_depends_on_its_callers() {
    let (mut r, client) = two_apis(("/a", "GET"), ("/b", "GET"));
    r.fact(&client, fact::API_CALL, "/a", "GET", 3, true);
    let res = run(&r);
    let deps: Vec<(&str, &str)> = res
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::DependsOn)
        .map(|e| (e.from.as_str(), e.to.as_str()))
        .collect();
    assert_eq!(deps, [(service_id("web").as_str(), service_id("a_api").as_str())]);
}

// ---------------------------------------------------------------------------
// Derived edges and placeholders
// ---------------------------------------------------------------------------

#[test]
fn dependencies_through_shared_nodes_follow_the_dependency_direction() {
    let mut r = Repo::new();
    r.file("w/a.py", "writer");
    r.file("r/a.py", "reader");
    r.file("p/a.py", "publisher");
    r.file("c/a.py", "consumer");
    let w = r.function("w/a.py", "w");
    let rd = r.function("r/a.py", "rd");
    let p = r.function("p/a.py", "p");
    let c = r.function("c/a.py", "c");
    r.fact(&w, fact::DB_WRITE, "t", "", 1, true);
    r.fact(&rd, fact::DB_READ, "T", "", 1, true);
    r.fact(&p, fact::QUEUE_PUBLISH, "q", "queue", 1, true);
    r.fact(&c, fact::QUEUE_CONSUME, "q", "queue", 1, true);
    // Two writers of one table, two publishers of one queue: no dependency between them.
    r.fact(&p, fact::DB_WRITE, "t", "", 2, true);
    r.fact(&w, fact::QUEUE_PUBLISH, "q", "queue", 2, true);
    let res = run(&r);
    let deps: BTreeSet<(String, String)> = res
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::DependsOn)
        .map(|e| {
            assert_eq!((e.method.as_str(), e.confidence), ("static", 0.90));
            (e.from.clone(), e.to.clone())
        })
        .collect();
    let wanted: BTreeSet<(String, String)> = [
        ("reader", "writer"),
        ("reader", "publisher"),
        ("consumer", "publisher"),
        ("consumer", "writer"),
    ]
    .into_iter()
    .map(|(a, b)| (service_id(a), service_id(b)))
    .collect();
    assert_eq!(deps, wanted);
    assert!(edge(&res, EdgeType::ReadsFrom, &rd, &table_id("t")).is_some());
    assert!(edge(&res, EdgeType::Consumes, &c, &queue_id("q")).is_some());
}

#[test]
fn a_placeholder_is_one_node_however_many_edges_reach_it() {
    let mut r = Repo::new();
    r.file("a/x.ts", "a");
    r.file("b/x.ts", "b");
    r.file("c/app.ts", "c");
    r.class("a/x.ts", "Thing");
    r.class("b/x.ts", "Thing");
    r.function("a/x.ts", "handler");
    r.function("b/x.ts", "handler");
    let ha = r.id("a.x.handler");
    let hb = r.id("b.x.handler");
    r.fact(&ha, fact::API_EXPOSE, "/x/one", "GET", 2, true);
    r.fact(&hb, fact::API_EXPOSE, "/x/two", "GET", 2, true);
    let child = r.class("c/app.ts", "Child");
    let other = r.class("c/app.ts", "Other");
    let main = r.function("c/app.ts", "main");
    let again = r.function("c/app.ts", "again");
    r.fact(&child, fact::INHERITS, "Thing", "", 3, true);
    r.fact(&other, fact::IMPLEMENTS, "Thing", "", 4, true);
    r.call(&main, "", "Thing", 9);
    r.call(&again, "", "Thing", 12);
    r.call(&again, "x", "Thing", 13);
    r.fact(&main, fact::API_CALL, "/x/${which}", "GET", 10, false);
    r.fact(&again, fact::API_CALL, "${BASE}/x/${other}", "get", 14, false);
    let res = run(&r);
    let placeholders: Vec<(NodeLabel, &str)> = res
        .nodes
        .iter()
        .filter(|n| n.kind == kind::UNRESOLVED)
        .map(|n| (n.label, n.id.as_str()))
        .collect();
    assert_eq!(
        placeholders,
        [
            (NodeLabel::Symbol, "unresolved:Thing"),
            (NodeLabel::ApiEndpoint, "unresolved:GET /x/{}"),
        ]
    );
    let unresolved: Vec<_> = res.edges.iter().filter(|e| e.method == "unresolved").collect();
    assert_eq!(unresolved.len(), 6, "{unresolved:#?}");
    assert_eq!(res.stats.total(EdgeType::DependsOn), 0);
    assert_eq!(res.stats.total(EdgeType::TestedBy), 0);
}

#[test]
fn a_call_site_without_a_file_takes_its_callers_file() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    let f = r.function("svc/a.py", "f");
    let g = r.function("svc/a.py", "g");
    r.call(&f, "", "g", 3);
    for call in &mut r.input.calls {
        call.file_path.clear();
    }
    let res = run(&r);
    let e = expect(&res, EdgeType::Calls, &f, &g, "static", 0.95);
    assert_eq!(e.evidence_file, "svc/a.py");
}

#[test]
fn a_test_calling_through_an_unresolved_name_tests_nothing() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    r.file("svc/b.py", "svc");
    r.test_file("svc/tests/test_a.py", "svc");
    r.function("svc/a.py", "build");
    r.function("svc/b.py", "build");
    let test = r.function("svc/tests/test_a.py", "test_build");
    r.call(&test, "", "build", 4);
    r.call(&test, "factory", "build", 5);
    let res = run(&r);
    assert_eq!(res.stats.get(EdgeType::Calls, "unresolved"), 1);
    assert_eq!(res.stats.total(EdgeType::TestedBy), 0);
}

// ---------------------------------------------------------------------------
// More guesses that must not pass for static edges
// ---------------------------------------------------------------------------

#[test]
fn two_definitions_of_an_imported_name_are_not_chosen_between() {
    let mut r = Repo::new();
    r.file("svc/app.py", "svc");
    r.file("svc/b.py", "svc");
    let main = r.function("svc/app.py", "main");
    // `b` has a function and a class called `make`, and two classes called `Repo`
    // (one nested): the import names one of them, the resolver cannot tell which.
    let make_function = r.function("svc/b.py", "make");
    r.class("svc/b.py", "make");
    r.class("svc/b.py", "Repo");
    r.method("svc/b.py", "Repo", "save_all");
    r.class("svc/b.py", "Outer");
    r.def("svc/b.py", "Outer.Repo", kind::CLASS);
    r.def("svc/b.py", "Outer.Repo.save_all", kind::METHOD);
    r.import("svc/app.py", ".b", "make", "make");
    r.import("svc/app.py", ".b", "Repo", "Repo");
    r.import("svc/app.py", ".", "b", "b");
    r.call(&main, "", "make", 3);
    r.call(&main, "b", "make", 4);
    r.call(&main, "Repo()", "save_all", 5);
    r.call(&main, "", "Repo", 5);
    let res = run(&r);
    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 0, "{:#?}", res.edges);
    // Through a receiver only a function or a method is a candidate by name, so
    // `b.make()` has one; it is still a guess.
    expect(&res, EdgeType::Calls, &main, &make_function, "inferred", 0.40);
    assert_eq!(res.stats.get(EdgeType::Calls, "inferred"), 1, "{:#?}", res.edges);
    for name in ["make", "Repo", "save_all"] {
        expect(
            &res,
            EdgeType::Calls,
            &main,
            &format!("unresolved:{name}"),
            "unresolved",
            0.0,
        );
    }
}

#[test]
fn the_original_name_of_an_aliased_import_is_not_bound() {
    let mut r = Repo::new();
    r.file("svc/app.py", "svc");
    r.file("svc/b.py", "svc");
    r.file("web/app.ts", "web");
    r.file("web/x.ts", "web");
    let py_main = r.function("svc/app.py", "main");
    let py_target = r.function("svc/b.py", "compute_total");
    let ts_main = r.function("web/app.ts", "main");
    let ts_target = r.function("web/x.ts", "formatTotal");
    r.import("svc/app.py", ".b", "compute_total", "total");
    r.import("web/app.ts", "./x", "formatTotal", "fmt");
    // The files bind `total` and `fmt`; the original names are not in scope.
    r.call(&py_main, "", "compute_total", 3);
    r.call(&ts_main, "", "formatTotal", 3);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &py_main, &py_target, "inferred", 0.50);
    expect(&res, EdgeType::Calls, &ts_main, &ts_target, "inferred", 0.50);
}

#[test]
fn a_default_import_never_falls_back_to_a_method_or_to_two_callables() {
    let mut r = Repo::new();
    for path in ["web/app.ts", "web/one.ts", "web/two.ts"] {
        r.file(path, "web");
    }
    let main = r.function("web/app.ts", "main");
    // No default export is marked in either file.
    r.class("web/one.ts", "Service");
    r.method("web/one.ts", "Service", "send");
    r.function("web/two.ts", "build");
    r.class("web/two.ts", "build");
    r.import("web/app.ts", "./one", "default", "send");
    r.import("web/app.ts", "./two", "default", "build");
    r.call(&main, "", "send", 3);
    r.call(&main, "", "build", 4);
    let res = run(&r);
    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 0, "{:#?}", res.edges);
    // `send` is a method (not a bare callable at all); `build` has two candidates.
    expect(&res, EdgeType::Calls, &main, "unresolved:build", "unresolved", 0.0);
    assert_eq!(res.stats.calls_external, 1);
}

#[test]
fn an_exact_route_two_services_expose_is_not_rescued_by_a_parameter_route() {
    let (mut r, client) = two_apis(("/orders/pending", "GET"), ("/orders/pending", "ANY"));
    r.file("c_api/routes.py", "c_api");
    let by_id = r.function("c_api/routes.py", "by_id");
    r.fact(&by_id, fact::API_EXPOSE, "/orders/{id}", "GET", 4, true);
    r.fact(&client, fact::API_CALL, "/orders/pending", "GET", 3, true);
    r.fact(&client, fact::API_CALL, "/orders/77", "GET", 5, true);
    let res = run(&r);
    // The written path fits two endpoints of two services; the parameter route
    // of a third is not a way out.
    expect(
        &res,
        EdgeType::CallsApi,
        &client,
        "unresolved:GET /orders/pending",
        "unresolved",
        0.0,
    );
    expect(
        &res,
        EdgeType::CallsApi,
        &client,
        &endpoint_id("GET", "/orders/{}"),
        "inferred",
        0.60,
    );
    let deps: Vec<&str> = edges_from(&res, EdgeType::DependsOn, &service_id("web"))
        .iter()
        .map(|e| e.to.as_str())
        .collect();
    assert_eq!(deps, [service_id("c_api").as_str()]);
}

#[test]
fn a_relative_import_takes_the_package_before_the_module_of_the_same_name() {
    let mut r = Repo::new();
    r.file("pkg/app.py", "pkg");
    r.file("pkg/conf.py", "pkg");
    r.file("pkg/conf/__init__.py", "pkg");
    let main = r.function("pkg/app.py", "main");
    r.function("pkg/conf.py", "load");
    let load = r.function("pkg/conf/__init__.py", "load");
    r.import("pkg/app.py", ".conf", "load", "load");
    r.call(&main, "", "load", 3);
    let res = run(&r);
    // As the interpreter does: `pkg/conf/` shadows `pkg/conf.py`.
    expect(
        &res,
        EdgeType::Imports,
        &file_id("pkg/app.py"),
        &file_id("pkg/conf/__init__.py"),
        "static",
        0.90,
    );
    expect(&res, EdgeType::Calls, &main, &load, "static", 0.90);
    assert_eq!(res.stats.total(EdgeType::Imports), 1);
}

#[test]
fn a_default_export_marked_on_the_module_is_referenced_but_never_called_or_extended() {
    let mut r = Repo::new();
    r.file("web/app.ts", "web");
    let config_module = r.file("web/config.ts", "web");
    let main = r.function("web/app.ts", "main");
    let child = r.class("web/app.ts", "Child");
    // `export default { ... }`: the default export is the module's own value.
    r.fact(&config_module, fact::DEFAULT_EXPORT, "config", "", 12, true);
    r.import("web/app.ts", "./config", "default", "config");
    r.fact(&main, fact::REFERENCE, "config", "", 4, true);
    r.fact(&child, fact::INHERITS, "config", "", 5, true);
    r.call(&main, "", "config", 6);
    r.call(&main, "config", "load_all", 7);
    let res = run(&r);
    // Passing the imported value on mentions that module, and only that.
    expect(&res, EdgeType::References, &main, &config_module, "static", 0.90);
    assert_eq!(res.stats.total(EdgeType::Calls), 0);
    assert_eq!(res.stats.total(EdgeType::Inherits), 0);
    assert_eq!(res.stats.calls_external, 2);
    assert_eq!(res.edges.len(), 2, "IMPORTS and REFERENCES: {:#?}", res.edges);
}
