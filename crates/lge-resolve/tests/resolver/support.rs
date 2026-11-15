//! Hand-built resolver inputs and the checks every scenario must pass.

use lge_core::{
    endpoint_id, fact, file_id, kind, queue_id, service_id, stable_id, symbol_id, table_id, CallSite, EdgeRec,
    EdgeType, FactSite, FileRec, ImportSite, NodeLabel, NodeRec, ResolveInput, Resolved, SymbolRec,
};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Builds a `ResolveInput` the way the indexer would describe a repository.
#[derive(Default)]
pub struct Repo {
    pub input: ResolveInput,
    seq: i64,
}

fn lang_of(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        // A few of the languages read through a grammar and a tags query.
        "go" => "go",
        "rs" => "rust",
        "rb" => "ruby",
        "java" => "java",
        "kt" => "kotlin",
        "c" => "c",
        "cpp" => "cpp",
        "ex" => "elixir",
        _ => "",
    }
}

/// Qualified name of a file's module symbol, as `docs/ARCHITECTURE.md` defines it.
fn module_name(path: &str) -> String {
    let stem = match path.rsplit_once('.') {
        Some((stem, _)) => stem,
        None => path,
    };
    let stem = stem.trim_end_matches("/__init__").trim_end_matches("/index");
    stem.replace('/', ".")
}

impl Repo {
    pub fn new() -> Repo {
        Repo::default()
    }

    fn next(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    fn add_file(&mut self, path: &str, service: &str, is_test: bool) -> String {
        self.input.files.push(FileRec {
            id: file_id(path),
            path: path.to_string(),
            service: service.to_string(),
            lang: lang_of(path).to_string(),
            content_hash: String::new(),
        });
        let module = module_name(path);
        let id = symbol_id(path, &module, kind::MODULE);
        self.input.symbols.push(SymbolRec {
            id: id.clone(),
            name: module.rsplit('.').next().unwrap_or(&module).to_string(),
            qualified_name: module,
            kind: kind::MODULE.to_string(),
            file_path: path.to_string(),
            service: service.to_string(),
            start_line: 1,
            end_line: 100,
            is_test,
            ..SymbolRec::default()
        });
        id
    }

    /// Adds a file and its module symbol. Returns the module symbol's id.
    pub fn file(&mut self, path: &str, service: &str) -> String {
        self.add_file(path, service, false)
    }

    /// A test file: every symbol defined in it is a test symbol.
    pub fn test_file(&mut self, path: &str, service: &str) -> String {
        self.add_file(path, service, true)
    }

    /// Adds a symbol to a file that was added before. `scoped` is its path inside
    /// the module, for example `TaskService.create_task`.
    pub fn def(&mut self, path: &str, scoped: &str, kind: &str) -> String {
        let module = self
            .input
            .symbols
            .iter()
            .find(|s| s.file_path == path && s.kind == kind::MODULE)
            .unwrap_or_else(|| panic!("add the file {path} first"))
            .clone();
        let qualified_name = format!("{}.{scoped}", module.qualified_name);
        let id = symbol_id(path, &qualified_name, kind);
        let line = self.next() * 10;
        self.input.symbols.push(SymbolRec {
            id: id.clone(),
            name: scoped.rsplit('.').next().unwrap_or(scoped).to_string(),
            qualified_name,
            kind: kind.to_string(),
            file_path: path.to_string(),
            service: module.service.clone(),
            start_line: line,
            end_line: line + 5,
            is_test: module.is_test,
            ..SymbolRec::default()
        });
        id
    }

    pub fn function(&mut self, path: &str, name: &str) -> String {
        self.def(path, name, kind::FUNCTION)
    }

    pub fn class(&mut self, path: &str, name: &str) -> String {
        self.def(path, name, kind::CLASS)
    }

    pub fn method(&mut self, path: &str, class: &str, name: &str) -> String {
        self.def(path, &format!("{class}.{name}"), kind::METHOD)
    }

    /// A call `receiver.name()` (or `name()` when the receiver is empty) inside `caller`.
    pub fn call(&mut self, caller: &str, receiver: &str, name: &str, line: i64) {
        let file_path = self
            .input
            .symbols
            .iter()
            .find(|s| s.id == caller)
            .map(|s| s.file_path.clone())
            .unwrap_or_default();
        let seq = self.next().to_string();
        self.input.calls.push(CallSite {
            id: stable_id(&["call", caller, name, &seq]),
            caller_id: caller.to_string(),
            file_path,
            name: name.to_string(),
            receiver: receiver.to_string(),
            line,
        });
    }

    pub fn import(&mut self, path: &str, module: &str, name: &str, alias: &str) {
        self.input.imports.push(ImportSite {
            id: stable_id(&["import", path, module, name, alias]),
            file_path: path.to_string(),
            module: module.to_string(),
            name: name.to_string(),
            alias: alias.to_string(),
        });
    }

    pub fn fact(&mut self, symbol: &str, kind: &str, key: &str, detail: &str, line: i64, literal: bool) {
        let file_path = self
            .input
            .symbols
            .iter()
            .find(|s| s.id == symbol)
            .map(|s| s.file_path.clone())
            .unwrap_or_default();
        self.fact_in(&file_path, symbol, kind, key, detail, line, literal);
    }

    /// A fact written in `path` whose symbol id need not be a symbol of the input.
    #[allow(clippy::too_many_arguments)]
    pub fn fact_in(&mut self, path: &str, symbol: &str, kind: &str, key: &str, detail: &str, line: i64, literal: bool) {
        let seq = self.next().to_string();
        self.input.facts.push(FactSite {
            id: stable_id(&["fact", symbol, kind, key, &seq]),
            symbol_id: symbol.to_string(),
            file_path: path.to_string(),
            kind: kind.to_string(),
            key: key.to_string(),
            detail: detail.to_string(),
            line,
            end_line: line,
            literal,
        });
    }

    /// Id of the symbol with this qualified name.
    pub fn id(&self, qualified_name: &str) -> String {
        let found: Vec<&SymbolRec> = self
            .input
            .symbols
            .iter()
            .filter(|s| s.qualified_name == qualified_name && s.kind != kind::MODULE)
            .collect();
        assert_eq!(found.len(), 1, "{qualified_name}: {} symbols", found.len());
        found[0].id.clone()
    }
}

// ---------------------------------------------------------------------------
// Running and reading results
// ---------------------------------------------------------------------------

/// Resolve, then check what must hold for every input: honest provenance, a
/// sorted and consistent output, and the same output whatever the input order.
pub fn run(repo: &Repo) -> Resolved {
    let res = lge_resolve::resolve(&repo.input);
    check(&repo.input, &res);
    assert_same(&res, &lge_resolve::resolve(&repo.input), "a second run");
    for seed in 1..=4 {
        assert_same(
            &res,
            &lge_resolve::resolve(&shuffled(&repo.input, seed)),
            "shuffled input",
        );
    }
    res
}

pub fn assert_same(a: &Resolved, b: &Resolved, what: &str) {
    assert_eq!(a.nodes, b.nodes, "nodes differ on {what}");
    assert_eq!(a.edges, b.edges, "edges differ on {what}");
    assert_eq!(a.stats, b.stats, "stats differ on {what}");
}

/// The same records in another order. A small generator keeps the tests free of
/// dependencies and the order reproducible.
pub fn shuffled(input: &ResolveInput, seed: u64) -> ResolveInput {
    let mut out = input.clone();
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    shuffle(&mut out.files, &mut state);
    shuffle(&mut out.symbols, &mut state);
    shuffle(&mut out.calls, &mut state);
    shuffle(&mut out.imports, &mut state);
    shuffle(&mut out.facts, &mut state);
    out
}

fn shuffle<T>(items: &mut [T], state: &mut u64) {
    for i in (1..items.len()).rev() {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let j = (*state >> 33) as usize % (i + 1);
        items.swap(i, j);
    }
}

pub fn edge<'r>(res: &'r Resolved, edge_type: EdgeType, from: &str, to: &str) -> Option<&'r EdgeRec> {
    res.edges
        .iter()
        .find(|e| e.edge_type == edge_type && e.from == from && e.to == to)
}

/// The edge must exist with exactly this provenance.
pub fn expect<'r>(
    res: &'r Resolved,
    edge_type: EdgeType,
    from: &str,
    to: &str,
    method: &str,
    confidence: f64,
) -> &'r EdgeRec {
    let e = edge(res, edge_type, from, to)
        .unwrap_or_else(|| panic!("no {edge_type} edge {from} -> {to}; edges: {:#?}", res.edges));
    assert_eq!(
        (e.method.as_str(), e.confidence),
        (method, confidence),
        "{edge_type} {from} -> {to}"
    );
    e
}

pub fn edges_from<'r>(res: &'r Resolved, edge_type: EdgeType, from: &str) -> Vec<&'r EdgeRec> {
    res.edges
        .iter()
        .filter(|e| e.edge_type == edge_type && e.from == from)
        .collect()
}

pub fn node<'r>(res: &'r Resolved, label: NodeLabel, id: &str) -> &'r NodeRec {
    res.nodes
        .iter()
        .find(|n| n.label == label && n.id == id)
        .unwrap_or_else(|| panic!("no {label:?} node {id}; nodes: {:#?}", res.nodes))
}

// ---------------------------------------------------------------------------
// Invariants
// ---------------------------------------------------------------------------

pub fn check(input: &ResolveInput, res: &Resolved) {
    assert_sorted_and_unique(res);
    assert_provenance(res);
    assert_static_is_justified(input, res);
    assert_ends_exist(input, res);
    assert_stats_count_the_edges(res);
}

fn assert_sorted_and_unique(res: &Resolved) {
    for pair in res.nodes.windows(2) {
        assert!(
            (pair[0].label, &pair[0].id) < (pair[1].label, &pair[1].id),
            "nodes out of order: {pair:#?}"
        );
    }
    for pair in res.edges.windows(2) {
        let key = |e: &EdgeRec| (e.edge_type, e.from.clone(), e.to.clone());
        assert!(
            key(&pair[0]) < key(&pair[1]),
            "edges out of order or duplicated: {pair:#?}"
        );
    }
}

/// Every edge is in the confidence band of its method, has evidence, and an
/// unresolved edge points at a placeholder and nowhere else.
pub fn assert_provenance(res: &Resolved) {
    let placeholders: HashSet<&str> = res
        .nodes
        .iter()
        .filter(|n| n.kind == kind::UNRESOLVED)
        .map(|n| n.id.as_str())
        .collect();
    for e in &res.edges {
        assert_ne!(e.from, e.to, "self edge: {e:?}");
        assert!(!e.evidence_file.is_empty(), "no evidence file: {e:?}");
        assert!(e.start_line <= e.end_line, "line range: {e:?}");
        match e.method.as_str() {
            "static" => assert!((0.90..=0.95).contains(&e.confidence), "static band: {e:?}"),
            "inferred" => assert!((0.40..=0.60).contains(&e.confidence), "inferred band: {e:?}"),
            "unresolved" => assert_eq!(e.confidence, 0.0, "unresolved band: {e:?}"),
            other => panic!("unknown method {other}: {e:?}"),
        }
        assert_eq!(
            e.method == "unresolved",
            placeholders.contains(e.to.as_str()),
            "only unresolved edges point at placeholders: {e:?}"
        );
        assert!(
            !placeholders.contains(e.from.as_str()),
            "edge from a placeholder: {e:?}"
        );
    }
    for id in placeholders {
        assert!(id.starts_with("unresolved:"), "placeholder id {id}");
    }
}

/// The node a literal fact names, and the edges it may justify.
fn fact_target(f: &FactSite) -> Option<(String, &'static [EdgeType])> {
    let method = if f.detail.is_empty() { "ANY" } else { f.detail.as_str() };
    match f.kind.as_str() {
        fact::API_EXPOSE => Some((endpoint_id(method, &f.key), &[EdgeType::ExposesApi])),
        fact::DB_READ => Some((table_id(&f.key), &[EdgeType::ReadsFrom])),
        fact::DB_WRITE => Some((table_id(&f.key), &[EdgeType::WritesTo])),
        fact::TABLE_MODEL => Some((table_id(&f.key), &[EdgeType::ReadsFrom, EdgeType::WritesTo])),
        fact::QUEUE_PUBLISH => Some((queue_id(&f.key), &[EdgeType::Publishes])),
        fact::QUEUE_CONSUME => Some((queue_id(&f.key), &[EdgeType::Consumes])),
        _ => None,
    }
}

/// A static edge must follow from the source: the two symbols share a file, the
/// file of one imports the file of the other (directly or through one
/// re-exporting file), or a literal fact names the target. Worked out here
/// from the input, without the resolver's own rules.
pub fn assert_static_is_justified(input: &ResolveInput, res: &Resolved) {
    let file_of: HashMap<&str, &str> = input
        .symbols
        .iter()
        .map(|s| (s.id.as_str(), s.file_path.as_str()))
        .collect();
    let path_of: HashMap<&str, &str> = input.files.iter().map(|f| (f.id.as_str(), f.path.as_str())).collect();
    let imports: HashSet<(&str, &str)> = res
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Imports)
        .map(|e| (path_of[e.from.as_str()], path_of[e.to.as_str()]))
        .collect();
    let reaches = |a: &str, b: &str| {
        imports.contains(&(a, b))
            || imports
                .iter()
                .any(|(from, mid)| *from == a && imports.contains(&(*mid, b)))
    };
    for e in res.edges.iter().filter(|e| e.method == "static") {
        match e.edge_type {
            EdgeType::Calls | EdgeType::Inherits | EdgeType::Implements | EdgeType::References => {
                let (a, b) = (file_of[e.from.as_str()], file_of[e.to.as_str()]);
                assert!(a == b || reaches(a, b), "static edge between unrelated files: {e:?}");
                if e.confidence > 0.90 {
                    assert_eq!(a, b, "the same-file band across files: {e:?}");
                }
            }
            EdgeType::Imports => assert_eq!(e.confidence, 0.90, "{e:?}"),
            EdgeType::ExposesApi
            | EdgeType::ReadsFrom
            | EdgeType::WritesTo
            | EdgeType::Publishes
            | EdgeType::Consumes => {
                // The fact names the symbol, or names no symbol and sits in its file.
                let owns = |f: &FactSite| {
                    f.symbol_id == e.from
                        || (!file_of.contains_key(f.symbol_id.as_str()) && file_of[e.from.as_str()] == f.file_path)
                };
                let literal = input
                    .facts
                    .iter()
                    .filter(|f| f.literal && owns(f))
                    .any(|f| fact_target(f).is_some_and(|(id, types)| id == e.to && types.contains(&e.edge_type)));
                assert!(literal, "static edge without a literal fact: {e:?}");
            }
            EdgeType::CallsApi => panic!("CALLS_API is never static: {e:?}"),
            EdgeType::TestedBy => {
                let call = edge(res, EdgeType::Calls, &e.to, &e.from).expect("TESTED_BY without its call");
                assert_eq!(
                    (call.method.as_str(), call.confidence),
                    ("static", e.confidence),
                    "{e:?}"
                );
            }
            // Checked by the tests of the derived edges.
            EdgeType::DependsOn => {}
            EdgeType::Contains => panic!("the resolver does not write CONTAINS: {e:?}"),
        }
    }
}

/// Both ends of every edge are nodes the store will have: input symbols and
/// files, services named by them, or derived nodes of the right label.
fn assert_ends_exist(input: &ResolveInput, res: &Resolved) {
    let symbols: HashSet<&str> = input.symbols.iter().map(|s| s.id.as_str()).collect();
    let files: HashSet<&str> = input.files.iter().map(|f| f.id.as_str()).collect();
    let services: HashSet<String> = input.files.iter().map(|f| service_id(&f.service)).collect();
    let derived: HashSet<(NodeLabel, &str)> = res.nodes.iter().map(|n| (n.label, n.id.as_str())).collect();
    let symbol = |id: &str| symbols.contains(id) || derived.contains(&(NodeLabel::Symbol, id));
    let mut targeted: HashSet<(NodeLabel, &str)> = HashSet::new();
    for e in &res.edges {
        let (from_ok, to_label) = match e.edge_type {
            EdgeType::Imports => (files.contains(e.from.as_str()), None),
            EdgeType::DependsOn => (services.contains(&e.from), None),
            EdgeType::ExposesApi | EdgeType::CallsApi => {
                (symbols.contains(e.from.as_str()), Some(NodeLabel::ApiEndpoint))
            }
            EdgeType::ReadsFrom | EdgeType::WritesTo => (symbols.contains(e.from.as_str()), Some(NodeLabel::DbTable)),
            EdgeType::Publishes | EdgeType::Consumes => (symbols.contains(e.from.as_str()), Some(NodeLabel::Queue)),
            _ => (symbols.contains(e.from.as_str()), Some(NodeLabel::Symbol)),
        };
        assert!(from_ok, "unknown from node: {e:?}");
        let to_ok = match (e.edge_type, to_label) {
            (EdgeType::Imports, _) => files.contains(e.to.as_str()),
            (EdgeType::DependsOn, _) => services.contains(&e.to),
            (_, Some(NodeLabel::Symbol)) => symbol(&e.to),
            (_, Some(label)) => derived.contains(&(label, e.to.as_str())),
            (_, None) => false,
        };
        assert!(to_ok, "unknown to node: {e:?}");
        if let Some(label) = to_label {
            targeted.insert((label, e.to.as_str()));
        }
    }
    for n in &res.nodes {
        assert!(
            targeted.contains(&(n.label, n.id.as_str())),
            "derived node without an edge: {n:?}"
        );
    }
}

fn assert_stats_count_the_edges(res: &Resolved) {
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    for e in &res.edges {
        *tally.entry(format!("{}/{}", e.edge_type, e.method)).or_insert(0) += 1;
    }
    let mut counted = res.stats.counts.clone();
    counted.remove("CALLS_API/external");
    assert_eq!(counted, tally, "stats must count exactly the stored edges");
}
