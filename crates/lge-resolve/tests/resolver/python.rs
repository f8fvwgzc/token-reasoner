//! Python imports and calls: the rules ported from the original `Store::rebuild_edges`.

use crate::scenarios;
use crate::support::{edge, edges_from, expect, node, run, Repo};
use lge_core::{file_id, service_id, EdgeType, NodeLabel};

#[test]
fn original_fixture_gives_the_original_counts() {
    let repo = scenarios::original_fixture();
    let res = run(&repo);

    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 14);
    assert_eq!(res.stats.get(EdgeType::Calls, "inferred"), 0);
    assert_eq!(res.stats.get(EdgeType::Calls, "unresolved"), 0);
    assert_eq!(res.stats.calls_external, 3);
    assert_eq!(res.stats.get(EdgeType::Imports, "static"), 5);
    assert_eq!(res.stats.total(EdgeType::Imports), 5);
    assert_eq!(res.stats.imports_external, 0);
    assert!(
        res.nodes.is_empty(),
        "no placeholder, endpoint, table or queue: {:?}",
        res.nodes
    );

    let create_task = repo.id("work_svc.service.TaskService.create_task");
    let handler = repo.id("work_svc.controller.create_task_handler");
    let test = repo.id("work_svc.tests.test_service.test_create_task");
    let service = repo.id("work_svc.service.TaskService");
    // self call, module alias, imported class, method of an imported class.
    expect(
        &res,
        EdgeType::Calls,
        &create_task,
        &repo.id("work_svc.service.TaskService.validate_title"),
        "static",
        0.95,
    );
    expect(
        &res,
        EdgeType::Calls,
        &create_task,
        &repo.id("shared.util.slugify"),
        "static",
        0.90,
    );
    expect(
        &res,
        EdgeType::Calls,
        &create_task,
        &repo.id("work_svc.repository.TaskRepository"),
        "static",
        0.90,
    );
    let save = repo.id("work_svc.repository.TaskRepository.save_task");
    let e = expect(&res, EdgeType::Calls, &create_task, &save, "static", 0.90);
    assert_eq!(
        (e.evidence_file.as_str(), e.start_line, e.end_line),
        ("work_svc/service.py", 11, 11)
    );
    // Same file, and across services through an import.
    let verify = repo.id("auth_svc.tokens.verify_token");
    expect(
        &res,
        EdgeType::Calls,
        &verify,
        &repo.id("auth_svc.tokens.decode"),
        "static",
        0.95,
    );
    expect(&res, EdgeType::Calls, &handler, &verify, "static", 0.90);
    expect(&res, EdgeType::Calls, &handler, &create_task, "static", 0.90);
    expect(&res, EdgeType::Calls, &test, &create_task, "static", 0.90);

    let import = expect(
        &res,
        EdgeType::Imports,
        &file_id("work_svc/controller.py"),
        &file_id("auth_svc/tokens.py"),
        "static",
        0.90,
    );
    assert_eq!(import.evidence_file, "work_svc/controller.py");

    // Derived: the test covers what it calls, and work_svc needs the other two services.
    expect(&res, EdgeType::TestedBy, &create_task, &test, "static", 0.90);
    expect(&res, EdgeType::TestedBy, &service, &test, "static", 0.90);
    assert_eq!(res.stats.total(EdgeType::TestedBy), 2);
    expect(
        &res,
        EdgeType::DependsOn,
        &service_id("work_svc"),
        &service_id("shared"),
        "static",
        0.90,
    );
    expect(
        &res,
        EdgeType::DependsOn,
        &service_id("work_svc"),
        &service_id("auth_svc"),
        "static",
        0.90,
    );
    assert_eq!(res.stats.total(EdgeType::DependsOn), 2);
}

#[test]
fn self_call_is_a_member_of_the_same_class() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    r.class("svc/a.py", "A");
    let go = r.method("svc/a.py", "A", "go");
    let helper = r.method("svc/a.py", "A", "helper");
    r.class("svc/a.py", "B");
    r.method("svc/a.py", "B", "helper");
    r.call(&go, "self", "helper", 5);
    let res = run(&r);
    let e = expect(&res, EdgeType::Calls, &go, &helper, "static", 0.95);
    assert_eq!((e.evidence_file.as_str(), e.start_line), ("svc/a.py", 5));
    assert_eq!(res.edges.len(), 1);
}

#[test]
fn self_call_to_a_member_the_class_lacks_is_matched_by_name() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    r.file("svc/base.py", "svc");
    r.file("svc/other.py", "svc");
    r.class("svc/a.py", "A");
    let go = r.method("svc/a.py", "A", "go");
    r.class("svc/base.py", "Base");
    let persist = r.method("svc/base.py", "Base", "persist");
    r.method("svc/base.py", "Base", "validate");
    r.class("svc/other.py", "Other");
    r.method("svc/other.py", "Other", "validate");
    r.method("svc/other.py", "Other", "update");
    r.call(&go, "self", "persist", 3);
    r.call(&go, "self", "validate", 4);
    r.call(&go, "self", "update", 5);
    let res = run(&r);
    // Defined once (an inherited method, as far as the resolver can tell): a guess.
    expect(&res, EdgeType::Calls, &go, &persist, "inferred", 0.40);
    // Defined twice: not chosen.
    expect(&res, EdgeType::Calls, &go, "unresolved:validate", "unresolved", 0.0);
    // Too common to guess at all.
    assert_eq!(res.stats.calls_external, 1);
    assert_eq!(edges_from(&res, EdgeType::Calls, &go).len(), 2);
}

#[test]
fn self_outside_a_class_is_never_static() {
    let mut r = Repo::new();
    r.file("svc/patch.py", "svc");
    let patched = r.function("svc/patch.py", "patched_save");
    let flush = r.function("svc/patch.py", "flush");
    r.call(&patched, "self", "flush", 3);
    let res = run(&r);
    // `self` is a plain parameter here: the module-level `flush` is a name match only.
    expect(&res, EdgeType::Calls, &patched, &flush, "inferred", 0.40);
}

#[test]
fn bare_call_resolves_in_the_same_file_then_through_an_import() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    r.file("svc/b.py", "svc");
    let f = r.function("svc/a.py", "f");
    let g = r.function("svc/a.py", "g");
    let h = r.function("svc/b.py", "h");
    let k = r.function("svc/b.py", "k");
    r.import("svc/a.py", ".b", "h", "h");
    r.import("svc/a.py", ".b", "k", "renamed");
    r.call(&f, "", "g", 3);
    r.call(&f, "", "h", 4);
    r.call(&f, "", "renamed", 5);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &f, &g, "static", 0.95);
    expect(&res, EdgeType::Calls, &f, &h, "static", 0.90);
    expect(&res, EdgeType::Calls, &f, &k, "static", 0.90);
    expect(
        &res,
        EdgeType::Imports,
        &file_id("svc/a.py"),
        &file_id("svc/b.py"),
        "static",
        0.90,
    );
    assert_eq!(
        res.stats.get(EdgeType::Imports, "static"),
        1,
        "two import sites, one edge"
    );
    assert_eq!(res.stats.imports_external, 0);
}

#[test]
fn a_definition_in_the_file_wins_over_an_import_of_the_same_name() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    r.file("svc/b.py", "svc");
    let f = r.function("svc/a.py", "f");
    let local = r.function("svc/a.py", "h");
    r.function("svc/b.py", "h");
    r.import("svc/a.py", ".b", "h", "h");
    r.call(&f, "", "h", 4);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &f, &local, "static", 0.95);
    assert_eq!(edges_from(&res, EdgeType::Calls, &f).len(), 1);
}

#[test]
fn call_on_an_imported_module_resolves_in_that_module() {
    let mut r = Repo::new();
    r.file("pkg/__init__.py", "pkg");
    r.file("pkg/mod.py", "pkg");
    r.file("app/main.py", "app");
    let target = r.function("pkg/mod.py", "f");
    let plain = r.function("app/main.py", "plain");
    let from = r.function("app/main.py", "from_import");
    let aliased = r.function("app/main.py", "aliased");
    r.import("app/main.py", "pkg.mod", "", "pkg.mod");
    r.import("app/main.py", "pkg", "mod", "mod");
    r.import("app/main.py", "pkg.mod", "", "m");
    r.call(&plain, "pkg.mod", "f", 3);
    r.call(&from, "mod", "f", 6);
    r.call(&aliased, "m", "f", 9);
    let res = run(&r);
    for caller in [&plain, &from, &aliased] {
        expect(&res, EdgeType::Calls, caller, &target, "static", 0.90);
    }
    assert_eq!(res.stats.get(EdgeType::Imports, "static"), 1);
}

#[test]
fn a_name_the_imported_module_lacks_is_matched_by_name_only() {
    let mut r = Repo::new();
    r.file("svc/main.py", "svc");
    r.file("svc/mod.py", "svc");
    r.file("svc/elsewhere.py", "svc");
    let main = r.function("svc/main.py", "main");
    let moved = r.function("svc/elsewhere.py", "moved");
    r.import("svc/main.py", ".", "mod", "mod");
    r.call(&main, "mod", "moved", 3);
    r.call(&main, "mod", "missing", 4);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &main, &moved, "inferred", 0.40);
    assert_eq!(res.stats.calls_external, 1);
}

#[test]
fn receiver_that_names_a_class_resolves_to_its_method() {
    let mut r = Repo::new();
    r.file("svc/repo.py", "svc");
    r.file("svc/service.py", "svc");
    r.class("svc/repo.py", "Repo");
    let save = r.method("svc/repo.py", "Repo", "save");
    let build = r.method("svc/repo.py", "Repo", "build");
    r.class("svc/service.py", "Local");
    let run_local = r.method("svc/service.py", "Local", "execute");
    let create = r.function("svc/service.py", "create");
    r.import("svc/service.py", ".repo", "Repo", "Repo");
    r.call(&create, "Repo()", "save", 3);
    r.call(&create, "Repo", "build", 4);
    r.call(&create, "Local()", "execute", 5);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &create, &save, "static", 0.90);
    expect(&res, EdgeType::Calls, &create, &build, "static", 0.90);
    expect(&res, EdgeType::Calls, &create, &run_local, "static", 0.90);
}

#[test]
fn single_candidate_by_name_is_inferred() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    r.file("svc/b.py", "svc");
    let f = r.function("svc/a.py", "f");
    let helper = r.function("svc/b.py", "helper");
    r.class("svc/b.py", "Store");
    let save_all = r.method("svc/b.py", "Store", "save_all");
    r.call(&f, "", "helper", 3);
    r.call(&f, "store", "save_all", 4);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &f, &helper, "inferred", 0.50);
    expect(&res, EdgeType::Calls, &f, &save_all, "inferred", 0.40);
    assert_eq!(res.stats.get(EdgeType::Calls, "inferred"), 2);
    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 0);
}

#[test]
fn a_bare_call_never_reaches_a_method_by_name() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    r.file("svc/b.py", "svc");
    let f = r.function("svc/a.py", "f");
    r.class("svc/b.py", "Store");
    r.method("svc/b.py", "Store", "save_all");
    r.call(&f, "", "save_all", 3);
    let res = run(&r);
    assert!(res.edges.is_empty());
    assert_eq!(res.stats.calls_external, 1);
}

#[test]
fn several_candidates_give_an_unresolved_edge_to_a_placeholder() {
    let mut r = Repo::new();
    r.file("a/views.py", "a");
    r.file("b/templates.py", "b");
    r.file("c/main.py", "c");
    r.function("a/views.py", "render");
    r.function("b/templates.py", "render");
    let main = r.function("c/main.py", "main");
    let other = r.function("c/main.py", "other");
    r.call(&main, "", "render", 3);
    r.call(&other, "tpl", "render", 8);
    let res = run(&r);
    let e = expect(&res, EdgeType::Calls, &main, "unresolved:render", "unresolved", 0.0);
    assert_eq!((e.evidence_file.as_str(), e.start_line), ("c/main.py", 3));
    expect(&res, EdgeType::Calls, &other, "unresolved:render", "unresolved", 0.0);
    let placeholder = node(&res, NodeLabel::Symbol, "unresolved:render");
    assert_eq!(
        (
            placeholder.name.as_str(),
            placeholder.qualified_name.as_str(),
            placeholder.kind.as_str()
        ),
        ("render", "render", "unresolved")
    );
    assert_eq!(res.nodes.len(), 1, "one placeholder per name");
    assert_eq!(res.stats.get(EdgeType::Calls, "unresolved"), 2);
    // An unknown target is not a dependency on either candidate's service.
    assert_eq!(res.stats.total(EdgeType::DependsOn), 0);
}

#[test]
fn unknown_and_too_common_names_are_external() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    r.file("svc/b.py", "svc");
    let f = r.function("svc/a.py", "f");
    r.class("svc/b.py", "Cache");
    r.method("svc/b.py", "Cache", "get");
    r.method("svc/b.py", "Cache", "run");
    r.method("svc/b.py", "Cache", "abc");
    let abc_function = r.function("svc/b.py", "xyz");
    r.call(&f, "", "print", 2);
    r.call(&f, "cache", "get", 3);
    r.call(&f, "cache", "run", 4);
    r.call(&f, "cache", "abc", 5);
    r.call(&f, "cache", "nowhere", 6);
    // The length rule is about receivers: a short bare name is still matched.
    r.call(&f, "", "xyz", 7);
    let res = run(&r);
    assert_eq!(res.stats.calls_external, 5);
    expect(&res, EdgeType::Calls, &f, &abc_function, "inferred", 0.50);
    assert_eq!(res.edges.len(), 1);
}

#[test]
fn one_edge_per_caller_and_callee_keeps_the_highest_confidence() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    r.class("svc/a.py", "A");
    let go = r.method("svc/a.py", "A", "go");
    let persist = r.method("svc/a.py", "A", "persist");
    r.call(&go, "other", "persist", 3);
    r.call(&go, "self", "persist", 9);
    r.call(&go, "self", "persist", 7);
    let res = run(&r);
    let e = expect(&res, EdgeType::Calls, &go, &persist, "static", 0.95);
    assert_eq!(e.start_line, 7, "equal confidence: the first call site is the evidence");
    assert_eq!(res.edges.len(), 1);
    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 1);
    assert_eq!(res.stats.get(EdgeType::Calls, "inferred"), 0);
}

#[test]
fn self_edges_are_dropped() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    let walk = r.function("svc/a.py", "walk");
    r.class("svc/a.py", "Tree");
    let visit = r.method("svc/a.py", "Tree", "visit");
    r.call(&walk, "", "walk", 3);
    r.call(&visit, "self", "visit", 8);
    let res = run(&r);
    assert!(res.edges.is_empty());
    assert_eq!(res.stats.calls_external, 0, "recursion is not an external call");
}

#[test]
fn relative_imports_follow_the_package_path() {
    let mut r = Repo::new();
    r.file("pkg/__init__.py", "pkg");
    r.file("pkg/c.py", "pkg");
    r.file("pkg/core/__init__.py", "pkg");
    r.file("pkg/sub/__init__.py", "pkg");
    r.file("pkg/sub/a.py", "pkg");
    r.file("pkg/sub/b.py", "pkg");
    let main = r.function("pkg/sub/a.py", "main");
    let in_b = r.function("pkg/sub/b.py", "in_b");
    let in_c = r.function("pkg/c.py", "in_c");
    let in_core = r.function("pkg/core/__init__.py", "in_core");
    r.import("pkg/sub/a.py", ".", "b", "b");
    r.import("pkg/sub/a.py", "..", "c", "c");
    r.import("pkg/sub/a.py", "..core", "in_core", "in_core");
    r.import("pkg/sub/a.py", "...outside", "x", "x");
    r.call(&main, "b", "in_b", 5);
    r.call(&main, "c", "in_c", 6);
    r.call(&main, "", "in_core", 7);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &main, &in_b, "static", 0.90);
    expect(&res, EdgeType::Calls, &main, &in_c, "static", 0.90);
    expect(&res, EdgeType::Calls, &main, &in_core, "static", 0.90);
    let from = file_id("pkg/sub/a.py");
    for target in ["pkg/sub/b.py", "pkg/c.py", "pkg/core/__init__.py"] {
        expect(&res, EdgeType::Imports, &from, &file_id(target), "static", 0.90);
    }
    assert_eq!(res.stats.imports_external, 1);
}

#[test]
fn a_module_name_shared_by_two_services_resolves_to_the_importers_own() {
    let mut r = Repo::new();
    r.file("a/utils.py", "a");
    r.file("a/main.py", "a");
    r.file("b/utils.py", "b");
    r.file("c/main.py", "c");
    let a_helper = r.function("a/utils.py", "helper");
    r.function("b/utils.py", "helper");
    let a_main = r.function("a/main.py", "main");
    let c_main = r.function("c/main.py", "main");
    r.import("a/main.py", "utils", "", "utils");
    r.import("c/main.py", "utils", "", "utils");
    r.call(&a_main, "utils", "helper", 3);
    r.call(&c_main, "utils", "helper", 3);
    let res = run(&r);
    expect(
        &res,
        EdgeType::Imports,
        &file_id("a/main.py"),
        &file_id("a/utils.py"),
        "static",
        0.90,
    );
    expect(&res, EdgeType::Calls, &a_main, &a_helper, "static", 0.90);
    // From a third service nothing says which `utils` is meant.
    assert_eq!(edges_from(&res, EdgeType::Imports, &file_id("c/main.py")).len(), 0);
    assert_eq!(res.stats.imports_external, 1);
    expect(&res, EdgeType::Calls, &c_main, "unresolved:helper", "unresolved", 0.0);
}

#[test]
fn a_name_imported_from_two_places_is_not_resolved_through_either() {
    let mut r = Repo::new();
    r.file("svc/app.py", "svc");
    r.file("svc/fast.py", "svc");
    r.file("svc/slow.py", "svc");
    let main = r.function("svc/app.py", "main");
    r.function("svc/fast.py", "parse");
    r.function("svc/slow.py", "parse");
    r.import("svc/app.py", ".fast", "parse", "parse");
    r.import("svc/app.py", ".slow", "parse", "parse");
    r.call(&main, "", "parse", 6);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &main, "unresolved:parse", "unresolved", 0.0);
    assert_eq!(edges_from(&res, EdgeType::Calls, &main).len(), 1);
    assert_eq!(
        res.stats.get(EdgeType::Imports, "static"),
        2,
        "both imports are still dependencies"
    );
}

#[test]
fn a_module_is_never_a_call_target() {
    let mut r = Repo::new();
    r.file("svc/tasks.py", "svc");
    r.file("svc/main.py", "svc");
    let main = r.function("svc/main.py", "main");
    r.import("svc/main.py", ".", "tasks", "tasks");
    r.call(&main, "", "tasks", 3);
    r.call(&main, "queue", "tasks", 4);
    let res = run(&r);
    assert_eq!(res.stats.total(EdgeType::Calls), 0);
    assert_eq!(res.stats.calls_external, 2);
}

#[test]
fn a_call_site_without_a_known_caller_is_skipped() {
    let mut r = Repo::new();
    r.file("svc/a.py", "svc");
    r.function("svc/a.py", "f");
    r.call("not-a-symbol", "", "f", 3);
    let res = run(&r);
    assert!(res.edges.is_empty());
    assert_eq!(res.stats.calls_external, 0);
    assert!(edge(&res, EdgeType::Calls, "not-a-symbol", &r.id("svc.a.f")).is_none());
}

#[test]
fn top_level_calls_belong_to_the_module_symbol() {
    let mut r = Repo::new();
    let module = r.file("svc/main.py", "svc");
    let setup = r.function("svc/main.py", "setup");
    r.call(&module, "", "setup", 12);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &module, &setup, "static", 0.95);
}
