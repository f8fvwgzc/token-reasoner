//! TypeScript and JavaScript: specifier resolution, import bindings, and the
//! same call ladder as Python.

use crate::support::{edges_from, expect, run, Repo};
use lge_core::{fact, file_id, kind, EdgeType};
use std::collections::BTreeSet;

#[test]
fn relative_specifiers_resolve_against_the_importing_directory() {
    let mut r = Repo::new();
    let files = [
        "web/src/app.ts",
        "web/src/util.ts",
        "web/src/util.js",
        "web/src/view.tsx",
        "web/src/legacy.js",
        "web/src/widget.jsx",
        "web/src/lib/index.ts",
        "web/src/both.ts",
        "web/src/both/index.ts",
        "web/src/exact.cjs",
        "web/src/compiled.ts",
        "web/src/tool.py",
        "web/shared/log.mjs",
        "web/index.js",
    ];
    for path in files {
        r.file(path, "web");
    }
    let imports = [
        ("./util", "web/src/util.ts"),            // .ts is tried before .js
        ("./view", "web/src/view.tsx"),           //
        ("./legacy", "web/src/legacy.js"),        //
        ("./widget", "web/src/widget.jsx"),       //
        ("./lib", "web/src/lib/index.ts"),        // a directory with an index file
        ("./both", "web/src/both.ts"),            // a file is tried before a directory
        ("./exact.cjs", "web/src/exact.cjs"),     // as written
        ("./compiled.js", "web/src/compiled.ts"), // TypeScript names the compiled file
        ("../shared/log", "web/shared/log.mjs"),  //
        ("..", "web/index.js"),                   //
        ("express", ""),                          // a package
        ("@scope/pkg/sub", ""),                   //
        ("./missing", ""),                        // no such file
        ("./tool", ""),                           // not a script file
        ("../../../outside", ""),                 // above the indexed root
    ];
    for (n, (specifier, _)) in imports.iter().enumerate() {
        r.import("web/src/app.ts", specifier, "*", &format!("ns{n}"));
    }
    let res = run(&r);
    let found: BTreeSet<String> = edges_from(&res, EdgeType::Imports, &file_id("web/src/app.ts"))
        .iter()
        .map(|e| e.to.clone())
        .collect();
    let wanted: BTreeSet<String> = imports
        .iter()
        .filter(|(_, to)| !to.is_empty())
        .map(|(_, to)| file_id(to))
        .collect();
    assert_eq!(found, wanted);
    assert_eq!(res.stats.get(EdgeType::Imports, "static"), 10);
    assert_eq!(res.stats.imports_external, 5);
    for e in &res.edges {
        assert_eq!(
            (e.method.as_str(), e.confidence, e.evidence_file.as_str()),
            ("static", 0.90, "web/src/app.ts")
        );
    }
}

#[test]
fn python_and_script_imports_do_not_cross() {
    let mut r = Repo::new();
    r.file("svc/main.py", "svc");
    r.file("svc/routes.ts", "svc");
    r.file("svc/app.ts", "svc");
    r.file("svc/helpers.py", "svc");
    r.import("svc/main.py", "routes", "", "routes");
    r.import("svc/main.py", ".", "routes", "routes");
    r.import("svc/app.ts", "./helpers", "*", "helpers");
    let res = run(&r);
    assert!(res.edges.is_empty());
    assert_eq!(res.stats.imports_external, 3);
}

#[test]
fn named_import_call_is_static() {
    let mut r = Repo::new();
    r.file("notifier/src/consumer.ts", "notifier");
    r.file("notifier/src/mailer.ts", "notifier");
    let handle = r.function("notifier/src/consumer.ts", "handle");
    let local = r.function("notifier/src/consumer.ts", "format");
    let send = r.function("notifier/src/mailer.ts", "sendEmail");
    let queue = r.function("notifier/src/mailer.ts", "enqueue");
    r.import("notifier/src/consumer.ts", "./mailer", "sendEmail", "sendEmail");
    r.import("notifier/src/consumer.ts", "./mailer", "enqueue", "later");
    r.call(&handle, "", "format", 4);
    r.call(&handle, "", "sendEmail", 5);
    r.call(&handle, "", "later", 6);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &handle, &local, "static", 0.95);
    expect(&res, EdgeType::Calls, &handle, &send, "static", 0.90);
    expect(&res, EdgeType::Calls, &handle, &queue, "static", 0.90);
    expect(
        &res,
        EdgeType::Imports,
        &file_id("notifier/src/consumer.ts"),
        &file_id("notifier/src/mailer.ts"),
        "static",
        0.90,
    );
}

#[test]
fn default_import_resolves_to_the_marked_default_export() {
    let mut r = Repo::new();
    r.file("web/app.ts", "web");
    r.file("web/mailer.ts", "web");
    let main = r.function("web/app.ts", "main");
    let deliver = r.function("web/mailer.ts", "deliver");
    // Same name as the importer's alias, but not what the file exports as default.
    r.function("web/mailer.ts", "send");
    r.fact(&deliver, fact::DEFAULT_EXPORT, "deliver", "", 9, true);
    r.import("web/app.ts", "./mailer", "default", "send");
    r.call(&main, "", "send", 4);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &main, &deliver, "static", 0.90);
    assert_eq!(res.stats.total(EdgeType::Calls), 1);
    // The fact itself is not an edge.
    assert_eq!(res.edges.len(), 2, "one CALLS and one IMPORTS: {:#?}", res.edges);
}

#[test]
fn default_import_without_a_marked_export_takes_the_symbol_named_like_the_alias() {
    let mut r = Repo::new();
    r.file("web/app.ts", "web");
    r.file("web/views.ts", "web");
    r.file("web/other.ts", "web");
    let main = r.function("web/app.ts", "main");
    let render = r.function("web/views.ts", "render");
    r.function("web/other.ts", "helper");
    r.import("web/app.ts", "./views", "default", "render");
    r.import("web/app.ts", "./other", "default", "anything");
    r.call(&main, "", "render", 4);
    r.call(&main, "", "anything", 5);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &main, &render, "static", 0.90);
    assert_eq!(res.stats.total(EdgeType::Calls), 1);
    assert_eq!(res.stats.calls_external, 1);
}

#[test]
fn default_export_that_is_not_callable_blocks_the_alias_fallback() {
    let mut r = Repo::new();
    r.file("web/app.ts", "web");
    let config_module = r.file("web/config.ts", "web");
    let main = r.function("web/app.ts", "main");
    let load = r.function("web/config.ts", "load");
    // `export default { ... }`: the default export is the module's own value.
    r.fact(&config_module, fact::DEFAULT_EXPORT, "config", "", 12, true);
    r.import("web/app.ts", "./config", "default", "load");
    r.call(&main, "", "load", 4);
    let res = run(&r);
    // `load()` calls the default export, which is not the function `load`.
    expect(&res, EdgeType::Calls, &main, &load, "inferred", 0.50);
}

#[test]
fn namespace_import_and_require_bind_a_module() {
    let mut r = Repo::new();
    r.file("svc/app.ts", "svc");
    r.file("svc/main.js", "svc");
    r.file("svc/audit.ts", "svc");
    let app = r.function("svc/app.ts", "app");
    let main = r.function("svc/main.js", "main");
    let record = r.function("svc/audit.ts", "record");
    r.import("svc/app.ts", "./audit", "*", "audit");
    r.import("svc/main.js", "./audit", "*", "auditLog");
    r.call(&app, "audit", "record", 3);
    r.call(&main, "auditLog", "record", 3);
    r.call(&app, "audit", "missing", 4);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &app, &record, "static", 0.90);
    expect(&res, EdgeType::Calls, &main, &record, "static", 0.90);
    assert_eq!(res.stats.calls_external, 1);
}

#[test]
fn re_exports_are_followed_one_step() {
    let mut r = Repo::new();
    for path in [
        "lib/index.ts",
        "lib/a.ts",
        "lib/b.ts",
        "lib/c.ts",
        "lib/widget.ts",
        "app/main.ts",
    ] {
        r.file(path, "svc");
    }
    let a = r.function("lib/a.ts", "a");
    let inner = r.function("lib/b.ts", "inner");
    let c1 = r.function("lib/c.ts", "c1");
    let widget = r.class("lib/widget.ts", "WidgetImpl");
    let draw = r.method("lib/widget.ts", "WidgetImpl", "draw");
    r.fact(&widget, fact::DEFAULT_EXPORT, "WidgetImpl", "", 3, true);
    r.import("lib/index.ts", "./a", "a", "a"); // export { a } from "./a"
    r.import("lib/index.ts", "./b", "inner", "outer"); // export { inner as outer } from "./b"
    r.import("lib/index.ts", "./c", "*", "*"); // export * from "./c"
    r.import("lib/index.ts", "./widget", "default", "Widget"); // export { default as Widget } from "./widget"
    let main = r.function("app/main.ts", "main");
    let via_namespace = r.function("app/main.ts", "viaNamespace");
    r.import("app/main.ts", "../lib", "a", "a");
    r.import("app/main.ts", "../lib", "outer", "outer");
    r.import("app/main.ts", "../lib", "c1", "c1");
    r.import("app/main.ts", "../lib", "Widget", "Widget");
    r.import("app/main.ts", "../lib", "*", "lib");
    r.call(&main, "", "a", 3);
    r.call(&main, "", "outer", 4);
    r.call(&main, "", "c1", 5);
    r.call(&main, "", "Widget", 6);
    r.call(&main, "Widget()", "draw", 6);
    r.call(&via_namespace, "lib", "a", 9);
    r.call(&via_namespace, "lib", "c1", 10);
    let res = run(&r);
    for callee in [&a, &inner, &c1, &widget, &draw] {
        expect(&res, EdgeType::Calls, &main, callee, "static", 0.90);
    }
    expect(&res, EdgeType::Calls, &via_namespace, &a, "static", 0.90);
    expect(&res, EdgeType::Calls, &via_namespace, &c1, "static", 0.90);
    // The importer depends on the barrel, the barrel on the files behind it.
    assert_eq!(edges_from(&res, EdgeType::Imports, &file_id("app/main.ts")).len(), 1);
    assert_eq!(edges_from(&res, EdgeType::Imports, &file_id("lib/index.ts")).len(), 4);
}

#[test]
fn a_second_re_export_step_is_not_followed() {
    let mut r = Repo::new();
    for path in ["lib/top.ts", "lib/mid.ts", "lib/leaf.ts", "app.ts"] {
        r.file(path, "svc");
    }
    let deep = r.function("lib/leaf.ts", "deep");
    r.import("lib/top.ts", "./mid", "deep", "deep");
    r.import("lib/mid.ts", "./leaf", "deep", "deep");
    r.import("app.ts", "./lib/top", "deep", "deep");
    let main = r.function("app.ts", "main");
    r.call(&main, "", "deep", 3);
    let res = run(&r);
    // Still found, but only by its name.
    expect(&res, EdgeType::Calls, &main, &deep, "inferred", 0.50);
}

#[test]
fn a_name_two_star_re_exports_provide_is_not_resolved_through_either() {
    let mut r = Repo::new();
    for path in ["lib/index.ts", "lib/x.ts", "lib/y.ts", "app.ts"] {
        r.file(path, "svc");
    }
    r.function("lib/x.ts", "dup");
    r.function("lib/y.ts", "dup");
    let only_x = r.function("lib/x.ts", "onlyX");
    r.import("lib/index.ts", "./x", "*", "*");
    r.import("lib/index.ts", "./y", "*", "");
    r.import("app.ts", "./lib", "dup", "dup");
    r.import("app.ts", "./lib", "onlyX", "onlyX");
    let main = r.function("app.ts", "main");
    r.call(&main, "", "dup", 3);
    r.call(&main, "", "onlyX", 4);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &main, "unresolved:dup", "unresolved", 0.0);
    expect(&res, EdgeType::Calls, &main, &only_x, "static", 0.90);
}

#[test]
fn a_definition_in_the_barrel_shadows_its_star_re_export() {
    let mut r = Repo::new();
    for path in ["lib/index.ts", "lib/x.ts", "app.ts"] {
        r.file(path, "svc");
    }
    // The barrel has its own `Thing` (an interface), so `Thing` from `./x` is hidden.
    r.def("lib/index.ts", "Thing", kind::INTERFACE);
    r.class("lib/x.ts", "Thing");
    r.import("lib/index.ts", "./x", "*", "*");
    r.import("app.ts", "./lib", "Thing", "Thing");
    let main = r.function("app.ts", "main");
    r.call(&main, "", "Thing", 3);
    let res = run(&r);
    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 0);
}

#[test]
fn this_call_is_a_member_of_the_same_class() {
    let mut r = Repo::new();
    r.file("notifier/src/audit.ts", "notifier");
    r.class("notifier/src/audit.ts", "AuditLog");
    let record = r.method("notifier/src/audit.ts", "AuditLog", "recordDelivery");
    let write = r.method("notifier/src/audit.ts", "AuditLog", "write");
    let free = r.function("notifier/src/audit.ts", "callback");
    r.function("notifier/src/audit.ts", "flushAll");
    r.call(&record, "self", "write", 7);
    r.call(&free, "self", "flushAll", 20);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &record, &write, "static", 0.95);
    // `this` in a free function is not a class instance.
    assert_eq!(res.stats.get(EdgeType::Calls, "static"), 1);
    assert_eq!(res.stats.get(EdgeType::Calls, "inferred"), 1);
}

#[test]
fn receiver_that_names_an_imported_class_resolves_to_its_method() {
    let mut r = Repo::new();
    r.file("svc/app.ts", "svc");
    r.file("svc/repo.ts", "svc");
    r.file("svc/cache.ts", "svc");
    let main = r.function("svc/app.ts", "main");
    r.class("svc/repo.ts", "Repo");
    let save = r.method("svc/repo.ts", "Repo", "save");
    let create = r.method("svc/repo.ts", "Repo", "create");
    let cache = r.class("svc/cache.ts", "LruCache");
    let evict = r.method("svc/cache.ts", "LruCache", "evict");
    r.fact(&cache, fact::DEFAULT_EXPORT, "LruCache", "", 1, true);
    r.import("svc/app.ts", "./repo", "Repo", "Repo");
    r.import("svc/app.ts", "./cache", "default", "Cache");
    r.call(&main, "Repo()", "save", 3); // new Repo().save()
    r.call(&main, "Repo", "create", 4); // Repo.create()
    r.call(&main, "Cache()", "evict", 5); // new Cache().evict()
    let res = run(&r);
    expect(&res, EdgeType::Calls, &main, &save, "static", 0.90);
    expect(&res, EdgeType::Calls, &main, &create, "static", 0.90);
    expect(&res, EdgeType::Calls, &main, &evict, "static", 0.90);
}

#[test]
fn interfaces_types_and_enums_are_never_call_targets() {
    let mut r = Repo::new();
    r.file("svc/types.ts", "svc");
    r.file("svc/app.ts", "svc");
    r.def("svc/types.ts", "Mailer", kind::INTERFACE);
    r.def("svc/types.ts", "Handler", kind::TYPE);
    r.def("svc/types.ts", "Colour", kind::ENUM);
    r.def("svc/app.ts", "Local", kind::INTERFACE);
    let main = r.function("svc/app.ts", "main");
    r.import("svc/app.ts", "./types", "Mailer", "Mailer");
    r.import("svc/app.ts", "./types", "*", "types");
    r.call(&main, "", "Mailer", 3);
    r.call(&main, "", "Handler", 4);
    r.call(&main, "", "Colour", 5);
    r.call(&main, "", "Local", 6);
    r.call(&main, "types", "Handler", 7);
    r.call(&main, "value", "Mailer", 8);
    let res = run(&r);
    assert_eq!(res.stats.total(EdgeType::Calls), 0, "{:#?}", res.edges);
    assert_eq!(res.stats.calls_external, 6);
}

#[test]
fn a_method_signature_in_an_interface_is_not_a_call_candidate() {
    let mut r = Repo::new();
    r.file("svc/mailer.ts", "svc");
    r.file("svc/app.ts", "svc");
    r.def("svc/mailer.ts", "Mailer", kind::INTERFACE);
    r.method("svc/mailer.ts", "Mailer", "deliver");
    r.class("svc/mailer.ts", "SmtpMailer");
    let deliver = r.method("svc/mailer.ts", "SmtpMailer", "deliver");
    let main = r.function("svc/app.ts", "main");
    r.call(&main, "mailer", "deliver", 4);
    let res = run(&r);
    // One implementation, not two candidates.
    expect(&res, EdgeType::Calls, &main, &deliver, "inferred", 0.40);
    assert_eq!(res.stats.total(EdgeType::Calls), 1);
}

#[test]
fn untyped_receivers_are_inferred_or_unresolved() {
    let mut r = Repo::new();
    r.file("notifier/src/consumer.ts", "notifier");
    r.file("notifier/src/audit.ts", "notifier");
    r.file("notifier/src/templates.ts", "notifier");
    r.file("web_gateway/src/views.ts", "web_gateway");
    let handle = r.function("notifier/src/consumer.ts", "handleOrderCreated");
    r.class("notifier/src/audit.ts", "AuditLog");
    let record = r.method("notifier/src/audit.ts", "AuditLog", "recordDelivery");
    r.function("notifier/src/templates.ts", "render");
    r.function("web_gateway/src/views.ts", "render");
    r.call(&handle, "audit", "recordDelivery", 13);
    r.call(&handle, "tpl", "render", 14);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &handle, &record, "inferred", 0.40);
    expect(&res, EdgeType::Calls, &handle, "unresolved:render", "unresolved", 0.0);
}
