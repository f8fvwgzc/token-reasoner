//! Randomly built repositories. Names, paths and keys come from small pools, so
//! collisions, ambiguous names, dangling ids and odd specifiers are the rule.
//! Every input goes through `support::run` (provenance, justified static edges,
//! the same output for shuffled input) and through the checks below, which are
//! worked out from the output and the input without the resolver's own rules.

use crate::support::{run, Repo};
use lge_core::{
    fact, kind, normalize_route, service_id, CallSite, EdgeRec, EdgeType, NodeLabel, ResolveInput, Resolved, SymbolRec,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// A small generator: reproducible, and no dependency for the tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        self.next() as usize % n.max(1)
    }

    fn pick<'p>(&mut self, pool: &[&'p str]) -> &'p str {
        pool[self.below(pool.len())]
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

const PATHS: &[&str] = &[
    "a/__init__.py",
    "a/util.py",
    "a/models.py",
    "a/tests/test_util.py",
    "b/util.py",
    "b/app.py",
    "b/a.py",
    "b/a/__init__.py",
    "main.py",
    "w/src/index.ts",
    "w/src/app.ts",
    "w/src/util.ts",
    "w/src/util/index.ts",
    "w/src.ts",
    "n/lib.js",
    "n/lib/index.js",
    "n/app.test.ts",
    "n/app.ts",
    "top.ts",
];
const NAMES: &[&str] = &["run_it", "helper", "Repo", "Base", "render", "Thing", "util"];
const MEMBERS: &[&str] = &["save_all", "helper", "render", "fetch_one", "get"];
const RECEIVERS: &[&str] = &[
    "", "", "", "self", "self", "Repo()", "Repo", "util", "ns", "obj", "Base", "lib", "dflt()", "a.util",
];
const PY_MODULES: &[&str] = &[
    ".", "..", "...", ".util", "..util", "...util", "util", "a.util", "a", "b.util", "models", "a.models", "b.a", "",
];
const TS_MODULES: &[&str] = &[
    ".",
    "..",
    "./",
    "../",
    "./util",
    "./util/",
    "../src",
    "./index",
    "./app",
    "../lib",
    "./lib",
    "./lib/",
    "express",
    "./util.js",
    "../../n/lib",
    "./src",
    "./src/util",
    "../../../x",
];
const IMPORT_NAMES: &[&str] = &["", "*", "default", "helper", "Repo", "Base", "render", "util", "models"];
const ALIASES: &[&str] = &["", "*", "util", "ns", "Repo", "lib", "dflt", "helper", "Base", "render"];
const ROUTES: &[&str] = &[
    "/orders",
    "/orders/{id}",
    "/orders/:id/",
    "/orders/pending",
    "/",
    "",
    "${B}/orders/${id}",
    "http://h/orders/7",
    "http://h",
    "/health",
    "/{x}",
    "${u}",
    "${B}/${p}",
];
const METHODS: &[&str] = &["GET", "POST", "", "ANY", "get"];
const TABLES: &[&str] = &["orders", "Orders", "users", " "];
const QUEUES: &[&str] = &["q1", "q2", ""];
const WRITTEN: &[&str] = &[
    "Base",
    "Repo",
    "helper",
    "ns.Base",
    "self.helper",
    "util.Repo",
    "Base<T>",
    "render",
    "Thing",
    "Repo.save_all",
    "<T>",
];
const FACT_KINDS: &[&str] = &[
    fact::API_EXPOSE,
    fact::API_CALL,
    fact::API_CALL,
    fact::DB_READ,
    fact::DB_WRITE,
    fact::TABLE_MODEL,
    fact::QUEUE_PUBLISH,
    fact::QUEUE_CONSUME,
    fact::INHERITS,
    fact::IMPLEMENTS,
    fact::REFERENCE,
    fact::REFERENCE,
    fact::DEFAULT_EXPORT,
    "no_such_kind",
];

fn service_of(path: &str) -> &str {
    match path.split_once('/') {
        Some((first, _)) => first,
        None => "",
    }
}

fn random_repo(seed: u64) -> Repo {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut r = Repo::new();
    let mut files: Vec<&str> = Vec::new();
    let mut owners: Vec<String> = Vec::new();
    for path in PATHS {
        if !rng.chance(60) {
            continue;
        }
        files.push(path);
        let is_test = path.contains("test");
        let module = if is_test {
            r.test_file(path, service_of(path))
        } else {
            r.file(path, service_of(path))
        };
        owners.push(module);
        let script = !path.ends_with(".py");
        let mut defined: HashSet<(String, &str)> = HashSet::new();
        for _ in 0..rng.below(5) {
            let name = rng.pick(NAMES);
            let symbol_kind = match rng.below(if script { 10 } else { 6 }) {
                0..=2 => kind::FUNCTION,
                3..=5 => kind::CLASS,
                6 | 7 => kind::INTERFACE,
                8 => kind::TYPE,
                _ => kind::ENUM,
            };
            if !defined.insert((name.to_string(), symbol_kind)) {
                continue;
            }
            owners.push(r.def(path, name, symbol_kind));
            if symbol_kind == kind::FUNCTION || symbol_kind == kind::ENUM {
                continue;
            }
            for _ in 0..rng.below(3) {
                let member = format!("{name}.{}", rng.pick(MEMBERS));
                if defined.insert((member.clone(), kind::METHOD)) {
                    owners.push(r.def(path, &member, kind::METHOD));
                }
            }
        }
    }
    if files.is_empty() {
        return r;
    }
    for line in 1..=rng.below(24) as i64 {
        let caller = if rng.chance(5) {
            "no-such-caller".to_string()
        } else {
            owners[rng.below(owners.len())].clone()
        };
        let name = if rng.chance(50) {
            rng.pick(NAMES)
        } else {
            rng.pick(MEMBERS)
        };
        r.call(&caller, rng.pick(RECEIVERS), name, line);
    }
    for _ in 0..rng.below(18) {
        let path = files[rng.below(files.len())];
        let module = if path.ends_with(".py") {
            rng.pick(PY_MODULES)
        } else {
            rng.pick(TS_MODULES)
        };
        let name = rng.pick(IMPORT_NAMES);
        let alias = if rng.chance(50) && !name.is_empty() {
            name
        } else {
            rng.pick(ALIASES)
        };
        r.import(path, module, name, alias);
    }
    for line in 1..=rng.below(24) as i64 {
        let fact_kind = rng.pick(FACT_KINDS);
        let (key, detail) = match fact_kind {
            fact::API_EXPOSE | fact::API_CALL => (rng.pick(ROUTES), rng.pick(METHODS)),
            fact::DB_READ | fact::DB_WRITE | fact::TABLE_MODEL => (rng.pick(TABLES), ""),
            fact::QUEUE_PUBLISH | fact::QUEUE_CONSUME => (rng.pick(QUEUES), rng.pick(&["queue", "topic", ""])),
            _ => (rng.pick(WRITTEN), ""),
        };
        let literal = rng.chance(60);
        if rng.chance(8) {
            let path = files[rng.below(files.len())];
            r.fact_in(path, "no-such-symbol", fact_kind, key, detail, line, literal);
        } else {
            let owner = owners[rng.below(owners.len())].clone();
            r.fact(&owner, fact_kind, key, detail, line, literal);
        }
    }
    // Now and then the store hands the same sites over twice.
    if rng.chance(15) {
        let again = r.input.clone();
        r.input.calls.extend(again.calls);
        r.input.imports.extend(again.imports);
        r.input.facts.extend(again.facts);
    }
    r
}

fn of_type(res: &Resolved, edge_type: EdgeType) -> impl Iterator<Item = &EdgeRec> {
    res.edges.iter().filter(move |e| e.edge_type == edge_type)
}

/// `TESTED_BY` is exactly the resolved calls from test symbols to other symbols, reversed.
fn check_tested_by(input: &ResolveInput, res: &Resolved) {
    let is_test: HashMap<&str, bool> = input.symbols.iter().map(|s| (s.id.as_str(), s.is_test)).collect();
    let wanted: BTreeSet<(&str, &str, &str, u64)> = of_type(res, EdgeType::Calls)
        .filter(|e| e.method != "unresolved")
        .filter(|e| is_test.get(e.from.as_str()) == Some(&true) && is_test.get(e.to.as_str()) == Some(&false))
        .map(|e| {
            (
                e.to.as_str(),
                e.from.as_str(),
                e.method.as_str(),
                e.confidence.to_bits(),
            )
        })
        .collect();
    let found: BTreeSet<(&str, &str, &str, u64)> = of_type(res, EdgeType::TestedBy)
        .map(|e| {
            (
                e.from.as_str(),
                e.to.as_str(),
                e.method.as_str(),
                e.confidence.to_bits(),
            )
        })
        .collect();
    assert_eq!(found, wanted, "TESTED_BY");
}

/// Only the edge types that have a placeholder may be unresolved, and each
/// placeholder is one node of the right label.
fn check_unresolved(res: &Resolved) {
    let mut count: BTreeMap<(NodeLabel, &str), usize> = BTreeMap::new();
    for n in res.nodes.iter().filter(|n| n.kind == kind::UNRESOLVED) {
        *count.entry((n.label, n.id.as_str())).or_insert(0) += 1;
    }
    for e in res.edges.iter().filter(|e| e.method == "unresolved") {
        let label = match e.edge_type {
            EdgeType::Calls | EdgeType::Inherits | EdgeType::Implements => NodeLabel::Symbol,
            EdgeType::CallsApi => NodeLabel::ApiEndpoint,
            other => panic!("{other} cannot be unresolved: {e:?}"),
        };
        assert_eq!(count.get(&(label, e.to.as_str())), Some(&1), "placeholder of {e:?}");
    }
}

/// A call or a base never lands on a symbol of the wrong kind.
fn check_target_kinds(input: &ResolveInput, res: &Resolved) {
    let kind_of: HashMap<&str, &str> = input.symbols.iter().map(|s| (s.id.as_str(), s.kind.as_str())).collect();
    for e in res.edges.iter().filter(|e| e.method != "unresolved") {
        let Some(k) = kind_of.get(e.to.as_str()) else { continue };
        match e.edge_type {
            EdgeType::Calls => assert!(
                [kind::FUNCTION, kind::METHOD, kind::CLASS].contains(k),
                "call to a {k}: {e:?}"
            ),
            EdgeType::Inherits | EdgeType::Implements => {
                assert!([kind::CLASS, kind::INTERFACE].contains(k), "base is a {k}: {e:?}")
            }
            _ => {}
        }
    }
}

/// `DEPENDS_ON` joins two different named services, is never unresolved, and is
/// static only when static edges alone carry it.
fn check_depends_on(input: &ResolveInput, res: &Resolved) {
    let symbol_service: HashMap<&str, &str> = input
        .symbols
        .iter()
        .map(|s| (s.id.as_str(), s.service.as_str()))
        .collect();
    let file_service: HashMap<&str, &str> = input
        .files
        .iter()
        .map(|f| (f.id.as_str(), f.service.as_str()))
        .collect();
    let service_of = |e: &EdgeRec, id: &str| -> String {
        let map = if e.edge_type == EdgeType::Imports {
            &file_service
        } else {
            &symbol_service
        };
        map.get(id).copied().unwrap_or("").to_string()
    };
    // (from service, to service) -> is some justification static?
    let mut wanted: BTreeMap<(String, String), bool> = BTreeMap::new();
    let mut propose = |from: String, to: String, is_static: bool| {
        if !from.is_empty() && !to.is_empty() && from != to {
            *wanted.entry((service_id(&from), service_id(&to))).or_insert(false) |= is_static;
        }
    };
    let resolved: Vec<&EdgeRec> = res.edges.iter().filter(|e| e.method != "unresolved").collect();
    for e in &resolved {
        if matches!(
            e.edge_type,
            EdgeType::Calls | EdgeType::Imports | EdgeType::Inherits | EdgeType::Implements | EdgeType::References
        ) {
            propose(service_of(e, &e.from), service_of(e, &e.to), e.method == "static");
        }
    }
    for (uses, provides) in [
        (EdgeType::CallsApi, EdgeType::ExposesApi),
        (EdgeType::Consumes, EdgeType::Publishes),
        (EdgeType::ReadsFrom, EdgeType::WritesTo),
    ] {
        for u in resolved.iter().filter(|e| e.edge_type == uses) {
            for p in resolved.iter().filter(|e| e.edge_type == provides && e.to == u.to) {
                propose(
                    service_of(u, &u.from),
                    service_of(p, &p.from),
                    u.method == "static" && p.method == "static",
                );
            }
        }
    }
    let found: BTreeMap<(String, String), bool> = of_type(res, EdgeType::DependsOn)
        .map(|e| {
            assert_ne!(e.method, "unresolved", "{e:?}");
            ((e.from.clone(), e.to.clone()), e.method == "static")
        })
        .collect();
    assert_eq!(found, wanted, "DEPENDS_ON pairs and whether each is static");
}

/// A stored `CALLS_API` edge is a guess at one endpoint of one service, and the
/// caller wrote at least one path segment of it.
fn check_api_calls(input: &ResolveInput, res: &Resolved) {
    let symbol_service: HashMap<&str, &str> = input
        .symbols
        .iter()
        .map(|s| (s.id.as_str(), s.service.as_str()))
        .collect();
    let symbols: HashSet<&str> = input.symbols.iter().map(|s| s.id.as_str()).collect();
    let file_of: HashMap<&str, &str> = input
        .symbols
        .iter()
        .map(|s| (s.id.as_str(), s.file_path.as_str()))
        .collect();
    for e in of_type(res, EdgeType::CallsApi).filter(|e| e.method != "unresolved") {
        assert_eq!(e.method, "inferred", "{e:?}");
        let exposing: BTreeSet<&str> = of_type(res, EdgeType::ExposesApi)
            .filter(|x| x.to == e.to)
            .map(|x| symbol_service.get(x.from.as_str()).copied().unwrap_or(""))
            .collect();
        assert_eq!(exposing.len(), 1, "endpoint of several services: {e:?}");
        let route = res
            .nodes
            .iter()
            .find(|n| n.label == NodeLabel::ApiEndpoint && n.id == e.to)
            .map(|n| n.qualified_name.clone())
            .unwrap_or_default();
        let declared: HashSet<&str> = route.split('/').filter(|s| !s.is_empty() && *s != "{}").collect();
        let written = input
            .facts
            .iter()
            .filter(|f| f.kind == fact::API_CALL)
            .filter(|f| {
                f.symbol_id == e.from
                    || (!symbols.contains(f.symbol_id.as_str())
                        && file_of.get(e.from.as_str()) == Some(&f.file_path.as_str()))
            })
            .any(|f| normalize_route(&f.key).split('/').any(|s| declared.contains(s)));
        assert!(written, "no api_call of the caller names a segment of {route}: {e:?}");
    }
}

/// What a call by name alone may reach: functions and classes for a bare call,
/// methods and functions through a receiver. A method declared only by an
/// interface or a type has no body to call.
fn name_candidates<'i>(input: &'i ResolveInput, name: &str, bare: bool) -> Vec<&'i SymbolRec> {
    let declared_only = |s: &SymbolRec| {
        let Some((scope, _)) = s.qualified_name.rsplit_once('.') else {
            return false;
        };
        let scopes: Vec<&SymbolRec> = input
            .symbols
            .iter()
            .filter(|o| o.file_path == s.file_path && o.qualified_name == scope && o.kind != kind::MODULE)
            .collect();
        !scopes.is_empty() && scopes.iter().all(|o| o.kind == kind::INTERFACE || o.kind == kind::TYPE)
    };
    input
        .symbols
        .iter()
        .filter(|s| s.name == name)
        .filter(|s| {
            if bare {
                s.kind == kind::FUNCTION || s.kind == kind::CLASS
            } else {
                (s.kind == kind::FUNCTION || s.kind == kind::METHOD) && !declared_only(s)
            }
        })
        .collect()
}

/// Every `CALLS` edge is traced back to a call site that justifies it:
/// * 0.95: `self.x()` inside the one class that has `x`, or a bare name with
///   exactly one callable of that name in the file;
/// * inferred: the name has exactly one candidate in the whole repository;
/// * unresolved: the name has at least two.
fn check_calls(input: &ResolveInput, res: &Resolved) {
    let by_id: HashMap<&str, &SymbolRec> = input.symbols.iter().map(|s| (s.id.as_str(), s)).collect();
    let in_file = |file: &str, pred: &dyn Fn(&SymbolRec) -> bool| -> Vec<&SymbolRec> {
        input
            .symbols
            .iter()
            .filter(|s| s.file_path == file && pred(s))
            .collect()
    };
    for e in of_type(res, EdgeType::Calls) {
        let caller = by_id[e.from.as_str()];
        let sites: Vec<&CallSite> = input.calls.iter().filter(|c| c.caller_id == caller.id).collect();
        assert!(!sites.is_empty(), "no call site: {e:?}");
        if e.method == "unresolved" {
            let name = e.to.trim_start_matches("unresolved:");
            let several = sites
                .iter()
                .any(|c| c.name == name && name_candidates(input, name, c.receiver.is_empty()).len() >= 2);
            assert!(several, "unresolved without two candidates: {e:?}");
            continue;
        }
        let callee = by_id[e.to.as_str()];
        if e.method == "inferred" {
            let confidence = |c: &CallSite| if c.receiver.is_empty() { 0.50 } else { 0.40 };
            let only = sites.iter().any(|c| {
                let found = name_candidates(input, &c.name, c.receiver.is_empty());
                confidence(c) == e.confidence && found.len() == 1 && found[0].id == callee.id
            });
            assert!(only, "inferred without a single candidate: {e:?}");
            continue;
        }
        if e.confidence < 0.95 {
            continue;
        }
        assert_eq!(caller.file_path, callee.file_path, "{e:?}");
        let file = caller.file_path.as_str();
        let justified = sites.iter().any(|c| {
            if c.receiver == "self" {
                let Some((scope, _)) = caller.qualified_name.rsplit_once('.') else {
                    return false;
                };
                let member = format!("{scope}.{}", c.name);
                let classes = in_file(file, &|s| s.qualified_name == scope && s.kind == kind::CLASS);
                let members = in_file(file, &|s| {
                    s.qualified_name == member && [kind::FUNCTION, kind::METHOD, kind::CLASS].contains(&s.kind.as_str())
                });
                classes.len() == 1 && members.len() == 1 && members[0].id == callee.id
            } else if c.receiver.is_empty() {
                let callables = in_file(file, &|s| {
                    s.name == c.name && (s.kind == kind::FUNCTION || s.kind == kind::CLASS)
                });
                callables.len() == 1 && callables[0].id == callee.id
            } else {
                false
            }
        });
        assert!(justified, "0.95 without a same-file justification: {e:?}");
    }
}

/// A static edge below the same-file band needs something in the caller's file
/// that binds the name: an import with that alias, or (for `Repo().save()`) a
/// class of that name in the file. A static base or reference needs a fact that
/// is written as a name.
fn check_static_bindings(input: &ResolveInput, res: &Resolved) {
    let by_id: HashMap<&str, &SymbolRec> = input.symbols.iter().map(|s| (s.id.as_str(), s)).collect();
    let binds = |file: &str, alias: &str| input.imports.iter().any(|i| i.file_path == file && i.alias == alias);
    for e in res.edges.iter().filter(|e| e.method == "static") {
        let from_file = by_id.get(e.from.as_str()).map(|s| s.file_path.as_str()).unwrap_or("");
        match e.edge_type {
            EdgeType::Calls if e.confidence < 0.95 => {
                let callee = by_id[e.to.as_str()];
                let bound = input.calls.iter().filter(|c| c.caller_id == e.from).any(|c| {
                    let class = c.receiver.trim_end_matches("()");
                    if c.receiver.is_empty() {
                        binds(from_file, &c.name)
                    } else if c.receiver == "self" {
                        false
                    } else {
                        binds(from_file, &c.receiver)
                            || binds(from_file, class)
                            || input.symbols.iter().any(|s| {
                                s.file_path == from_file
                                    && s.kind == kind::CLASS
                                    && s.name == class
                                    && callee.qualified_name == format!("{}.{}", s.qualified_name, c.name)
                            })
                    }
                });
                assert!(bound, "static call through nothing the file binds: {e:?}");
            }
            EdgeType::Inherits | EdgeType::Implements | EdgeType::References => {
                let fact_kind = match e.edge_type {
                    EdgeType::Inherits => fact::INHERITS,
                    EdgeType::Implements => fact::IMPLEMENTS,
                    _ => fact::REFERENCE,
                };
                let written = input.facts.iter().any(|f| {
                    let owns = f.symbol_id == e.from
                        || (!by_id.contains_key(f.symbol_id.as_str()) && f.file_path == from_file);
                    f.kind == fact_kind && f.literal && owns
                });
                assert!(written, "static {} without a literal fact: {e:?}", e.edge_type);
            }
            _ => {}
        }
    }
}

#[test]
fn random_repositories_keep_every_invariant() {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for seed in 0..1200 {
        let repo = random_repo(seed);
        let res = run(&repo);
        check_calls(&repo.input, &res);
        check_static_bindings(&repo.input, &res);
        check_tested_by(&repo.input, &res);
        check_unresolved(&res);
        check_target_kinds(&repo.input, &res);
        check_depends_on(&repo.input, &res);
        check_api_calls(&repo.input, &res);
        seen.extend(res.stats.counts.keys().cloned());
    }
    // The generator must reach every kind of outcome, or the checks above prove little.
    for key in [
        "CALLS/static",
        "CALLS/inferred",
        "CALLS/unresolved",
        "IMPORTS/static",
        "INHERITS/static",
        "INHERITS/unresolved",
        "IMPLEMENTS/inferred",
        "REFERENCES/static",
        "EXPOSES_API/static",
        "CALLS_API/inferred",
        "CALLS_API/unresolved",
        "CALLS_API/external",
        "READS_FROM/static",
        "WRITES_TO/inferred",
        "PUBLISHES/static",
        "CONSUMES/static",
        "TESTED_BY/static",
        "DEPENDS_ON/static",
        "DEPENDS_ON/inferred",
    ] {
        assert!(seen.contains(key), "no random repository produced {key}: {seen:?}");
    }
}
