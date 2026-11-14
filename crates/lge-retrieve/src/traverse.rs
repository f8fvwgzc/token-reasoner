//! Bounded traversals over the stored graph: change impact, execution flow,
//! one-hop dependencies, and turning a name into nodes.

use anyhow::Result;
use lge_core::{kind, method, Direction, EdgeRec, EdgeType, NodeLabel, NodeRec};
use lge_store::GraphStore;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Edge types where the `from` node depends on the `to` node, among the edges
/// that connect symbols and the things they touch. File- and service-level
/// edges (IMPORTS, DEPENDS_ON) and CONTAINS are not walked from symbols.
const FROM_DEPENDS: [EdgeType; 7] = [
    EdgeType::Calls,
    EdgeType::Inherits,
    EdgeType::Implements,
    EdgeType::References,
    EdgeType::CallsApi,
    EdgeType::ReadsFrom,
    EdgeType::Consumes,
];
/// Edge types where the `to` node depends on the `from` node.
const TO_DEPENDS: [EdgeType; 4] = [
    EdgeType::ExposesApi,
    EdgeType::WritesTo,
    EdgeType::Publishes,
    EdgeType::TestedBy,
];

/// Execution flows along these from `from` to `to`...
const FLOW_FORWARD: [EdgeType; 3] = [EdgeType::Calls, EdgeType::CallsApi, EdgeType::Publishes];
/// ...and along these from `to` to `from` (an endpoint runs its handler, a
/// queue feeds its consumer).
const FLOW_BACKWARD: [EdgeType; 2] = [EdgeType::ExposesApi, EdgeType::Consumes];

#[derive(Debug, Clone)]
pub struct WalkOptions {
    pub depth: u8,
    pub max_nodes: usize,
    /// A node that was not a start node and has at least this many neighbours
    /// is reported but not expanded: everything is near a hub.
    pub hub_degree: i64,
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions {
            depth: 3,
            max_nodes: 200,
            hub_degree: 50,
        }
    }
}

/// A node found by a walk, with how sure the path to it is.
#[derive(Debug, Clone, Serialize)]
pub struct Reached {
    pub node: NodeRec,
    pub hops: u8,
    /// Lowest confidence on the first path that reached it.
    pub confidence: f64,
    /// Weakest method on that path: a path through one inferred edge is inferred.
    pub method: String,
    /// The last edge of that path.
    pub via: EdgeRec,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Walk {
    /// Nearest first, then by qualified name.
    pub reached: Vec<Reached>,
    /// Every edge seen between walked nodes, unresolved ones excluded.
    pub edges: Vec<EdgeRec>,
    /// Unresolved edges met on the way. Their targets are not walked.
    pub unresolved: Vec<EdgeRec>,
    /// True when `max_nodes` stopped the walk.
    pub truncated: bool,
}

fn strength(m: &str) -> u8 {
    match m {
        method::STATIC => 2,
        method::INFERRED => 1,
        _ => 0,
    }
}

fn weaker<'a>(a: &'a str, b: &'a str) -> &'a str {
    if strength(b) < strength(a) {
        b
    } else {
        a
    }
}

/// One hop from `frontier`, as (next node id, edge).
type Step = fn(&dyn GraphStore, &[String]) -> Result<Vec<(String, EdgeRec)>>;

fn hop(
    store: &dyn GraphStore,
    frontier: &[String],
    out: &[EdgeType],
    inn: &[EdgeType],
) -> Result<Vec<(String, EdgeRec)>> {
    let mut next: Vec<(String, EdgeRec)> = Vec::new();
    for e in store.edges(frontier, out, Direction::Out)? {
        next.push((e.to.clone(), e));
    }
    for e in store.edges(frontier, inn, Direction::In)? {
        next.push((e.from.clone(), e));
    }
    next.sort_by(|a, b| {
        (&a.0, a.1.edge_type, &a.1.from, &a.1.to)
            .cmp(&(&b.0, b.1.edge_type, &b.1.from, &b.1.to))
            .then(b.1.confidence.total_cmp(&a.1.confidence))
    });
    Ok(next)
}

/// What the frontier depends on.
fn step_dependencies(store: &dyn GraphStore, frontier: &[String]) -> Result<Vec<(String, EdgeRec)>> {
    hop(store, frontier, &FROM_DEPENDS, &TO_DEPENDS)
}

/// What depends on the frontier.
fn step_dependents(store: &dyn GraphStore, frontier: &[String]) -> Result<Vec<(String, EdgeRec)>> {
    hop(store, frontier, &TO_DEPENDS, &FROM_DEPENDS)
}

fn step_downstream(store: &dyn GraphStore, frontier: &[String]) -> Result<Vec<(String, EdgeRec)>> {
    hop(store, frontier, &FLOW_FORWARD, &FLOW_BACKWARD)
}

fn step_upstream(store: &dyn GraphStore, frontier: &[String]) -> Result<Vec<(String, EdgeRec)>> {
    hop(store, frontier, &FLOW_BACKWARD, &FLOW_FORWARD)
}

/// Breadth-first walk from `start` using `step`, bounded by depth and node count.
fn walk(store: &dyn GraphStore, start: &[String], step: Step, opts: &WalkOptions) -> Result<Walk> {
    let mut out = Walk::default();
    // id -> (confidence, method) of the path that reached it.
    let mut path: HashMap<String, (f64, String)> = start
        .iter()
        .map(|id| (id.clone(), (1.0, method::STATIC.to_string())))
        .collect();
    let mut frontier: Vec<String> = start.to_vec();
    frontier.sort();
    frontier.dedup();
    let mut seen_edges: BTreeSet<(String, String, EdgeType)> = BTreeSet::new();

    for hops in 1..=opts.depth.clamp(1, 6) {
        if frontier.is_empty() || out.truncated {
            break;
        }
        // next id -> (confidence, method, via): the strongest way in at this hop.
        let mut found: BTreeMap<String, (f64, String, EdgeRec)> = BTreeMap::new();
        for (next, edge) in step(store, &frontier)? {
            if !seen_edges.insert((edge.from.clone(), edge.to.clone(), edge.edge_type)) {
                continue;
            }
            if edge.method == method::UNRESOLVED {
                out.unresolved.push(edge);
                continue;
            }
            let parent = if next == edge.to { &edge.from } else { &edge.to };
            let Some((parent_conf, parent_method)) = path.get(parent).cloned() else {
                continue;
            };
            out.edges.push(edge.clone());
            if path.contains_key(&next) {
                continue;
            }
            let conf = parent_conf.min(edge.confidence);
            let how = weaker(&parent_method, &edge.method).to_string();
            let better = found
                .get(&next)
                .is_none_or(|(c, m, _)| (strength(&how), conf) > (strength(m), *c));
            if better {
                found.insert(next, (conf, how, edge));
            }
        }
        let ids: Vec<String> = found.keys().cloned().collect();
        let mut nodes: Vec<NodeRec> = store
            .nodes(&ids)?
            .into_iter()
            .filter(|n| n.kind != kind::UNRESOLVED)
            .collect();
        nodes.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name).then(a.id.cmp(&b.id)));
        let hubs = hubs_among(store, &nodes, opts.hub_degree);
        frontier.clear();
        for node in nodes {
            if out.reached.len() >= opts.max_nodes {
                out.truncated = true;
                break;
            }
            let Some((confidence, how, via)) = found.remove(&node.id) else {
                continue;
            };
            path.insert(node.id.clone(), (confidence, how.clone()));
            if !hubs.contains(&node.id) {
                frontier.push(node.id.clone());
            }
            out.reached.push(Reached {
                node,
                hops,
                confidence,
                method: how,
                via,
            });
        }
    }
    Ok(out)
}

/// Ids among `nodes` whose stored degree is at or over the hub cut-off. A store
/// without metrics has no hubs.
fn hubs_among(store: &dyn GraphStore, nodes: &[NodeRec], hub_degree: i64) -> BTreeSet<String> {
    let ids: Vec<String> = nodes
        .iter()
        .filter(|n| n.label == NodeLabel::Symbol)
        .map(|n| n.id.clone())
        .collect();
    if ids.is_empty() {
        return BTreeSet::new();
    }
    store
        .metrics(&ids)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.degree >= hub_degree)
        .map(|m| m.id)
        .collect()
}

/// Everything the given nodes need in order to work: callees, base classes,
/// referenced symbols, endpoints they call, tables they read, queues they consume.
pub fn dependencies_of(store: &dyn GraphStore, ids: &[String], opts: &WalkOptions) -> Result<Walk> {
    walk(store, ids, step_dependencies, opts)
}

/// Everything that can be affected by changing the given nodes.
pub fn dependents_of(store: &dyn GraphStore, ids: &[String], opts: &WalkOptions) -> Result<Walk> {
    walk(store, ids, step_dependents, opts)
}

// ---------------------------------------------------------------------------
// Change impact
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct Impact {
    pub targets: Vec<NodeRec>,
    /// Production symbols that depend on the targets, nearest first.
    pub affected: Vec<Reached>,
    /// Tests that reach the targets. Run these after the change.
    pub tests: Vec<Reached>,
    pub endpoints: Vec<Reached>,
    pub tables: Vec<Reached>,
    pub queues: Vec<Reached>,
    /// Production files of the targets and of everything affected.
    pub files: Vec<String>,
    pub services: Vec<String>,
    /// Unresolved edges that leave the targets or were met on the way: places
    /// where the graph does not know what is connected.
    pub unresolved_edges: i64,
    /// Affected symbols that are only reached through an inferred edge.
    pub inferred_paths: usize,
    pub truncated: bool,
}

/// Who is affected by a change to `ids`: dependency edges walked backwards.
pub fn impact(store: &dyn GraphStore, ids: &[String], opts: &WalkOptions) -> Result<Impact> {
    let mut targets: Vec<NodeRec> = store
        .nodes(ids)?
        .into_iter()
        .filter(|n| n.kind != kind::UNRESOLVED)
        .collect();
    targets.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name).then(a.id.cmp(&b.id)));
    let start: Vec<String> = targets.iter().map(|n| n.id.clone()).collect();
    let found = dependents_of(store, &start, opts)?;

    let mut out = Impact {
        truncated: found.truncated,
        ..Impact::default()
    };
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut services: BTreeSet<String> = BTreeSet::new();
    let mut note = |n: &NodeRec| {
        if !n.is_test {
            if !n.file_path.is_empty() && n.label == NodeLabel::Symbol {
                files.insert(n.file_path.clone());
            }
            if !n.service.is_empty() {
                services.insert(n.service.clone());
            }
        }
    };
    out.unresolved_edges = found.unresolved.len() as i64;
    for t in &targets {
        note(t);
        if t.label == NodeLabel::Symbol {
            out.unresolved_edges += store.unresolved_out(&t.id)?;
        }
    }
    for r in found.reached {
        note(&r.node);
        match r.node.label {
            NodeLabel::ApiEndpoint => out.endpoints.push(r),
            NodeLabel::DbTable => out.tables.push(r),
            NodeLabel::Queue => out.queues.push(r),
            NodeLabel::Symbol if r.node.is_test => out.tests.push(r),
            NodeLabel::Symbol => {
                if r.method != method::STATIC {
                    out.inferred_paths += 1;
                }
                out.affected.push(r);
            }
            _ => {}
        }
    }
    out.targets = targets;
    out.files = files.into_iter().collect();
    out.services = services.into_iter().collect();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Execution flow
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Trace {
    pub root: NodeRec,
    /// True: what runs after the root (callees, called endpoints and their
    /// handlers, published queues and their consumers). False: what leads to it.
    pub downstream: bool,
    /// Nearest first. `via` is the edge that reached the node and carries the
    /// call site (`evidence_file`, `start_line`).
    pub steps: Vec<Reached>,
    pub unresolved: Vec<EdgeRec>,
    pub truncated: bool,
}

pub fn trace(store: &dyn GraphStore, id: &str, downstream: bool, opts: &WalkOptions) -> Result<Option<Trace>> {
    let Some(root) = store.nodes(&[id.to_string()])?.into_iter().next() else {
        return Ok(None);
    };
    let step: Step = if downstream { step_downstream } else { step_upstream };
    let found = walk(store, std::slice::from_ref(&root.id), step, opts)?;
    Ok(Some(Trace {
        root,
        downstream,
        steps: found.reached,
        unresolved: found.unresolved,
        truncated: found.truncated,
    }))
}

// ---------------------------------------------------------------------------
// One-hop dependencies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Deps {
    pub node: NodeRec,
    /// What the node depends on, with the edge that says so.
    pub uses: Vec<(EdgeRec, NodeRec)>,
    /// What depends on the node.
    pub used_by: Vec<(EdgeRec, NodeRec)>,
}

/// Upstream and downstream neighbours of one node (symbol, file, service,
/// endpoint, table or queue) over every edge type except CONTAINS.
pub fn dependencies(store: &dyn GraphStore, id: &str) -> Result<Option<Deps>> {
    let ids = [id.to_string()];
    let Some(node) = store.nodes(&ids)?.into_iter().next() else {
        return Ok(None);
    };
    let mut pairs: Vec<(bool, String, EdgeRec)> = Vec::new(); // (node uses other, other id, edge)
    for e in store.edges(&ids, &[], Direction::Out)? {
        if let Some(from_depends) = e.edge_type.from_depends_on_to() {
            pairs.push((from_depends, e.to.clone(), e));
        }
    }
    for e in store.edges(&ids, &[], Direction::In)? {
        if let Some(from_depends) = e.edge_type.from_depends_on_to() {
            pairs.push((!from_depends, e.from.clone(), e));
        }
    }
    let other_ids: Vec<String> = pairs
        .iter()
        .map(|p| p.1.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let others: HashMap<String, NodeRec> = store
        .nodes(&other_ids)?
        .into_iter()
        .map(|n| (n.id.clone(), n))
        .collect();
    let mut deps = Deps {
        node,
        uses: Vec::new(),
        used_by: Vec::new(),
    };
    for (uses, other, edge) in pairs {
        let Some(n) = others.get(&other) else { continue };
        if uses {
            deps.uses.push((edge, n.clone()));
        } else {
            deps.used_by.push((edge, n.clone()));
        }
    }
    let order = |a: &(EdgeRec, NodeRec), b: &(EdgeRec, NodeRec)| {
        (a.0.edge_type, &a.1.qualified_name, &a.1.id).cmp(&(b.0.edge_type, &b.1.qualified_name, &b.1.id))
    };
    deps.uses.sort_by(order);
    deps.used_by.sort_by(order);
    Ok(Some(deps))
}

// ---------------------------------------------------------------------------
// Names to nodes
// ---------------------------------------------------------------------------

/// Turn what a user or an agent typed into graph nodes. Tried in order: a
/// symbol id, qualified name or name; a file path (its symbols); a service
/// name; an endpoint (`GET /orders/{}`), table or queue name. The first kind
/// that matches wins.
pub fn resolve_targets(store: &dyn GraphStore, text: &str) -> Result<Vec<NodeRec>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let hits = store.find(text)?;
    if !hits.is_empty() {
        let ids: Vec<String> = hits.into_iter().map(|h| h.id).collect();
        let mut nodes = store.nodes(&ids)?;
        nodes.sort_by(|a, b| a.is_test.cmp(&b.is_test).then(a.qualified_name.cmp(&b.qualified_name)));
        return Ok(nodes);
    }
    // A node of any label by its id.
    let by_id = store.nodes(&[text.to_string()])?;
    if !by_id.is_empty() {
        return Ok(by_id);
    }
    let path = text.trim_start_matches("./");
    if store.file_info(path)?.is_some() {
        let symbols = store.symbols_in_file(path)?;
        let (modules, rest): (Vec<NodeRec>, Vec<NodeRec>) = symbols.into_iter().partition(|n| n.kind == kind::MODULE);
        return Ok(if rest.is_empty() { modules } else { rest });
    }
    for label in [
        NodeLabel::Service,
        NodeLabel::ApiEndpoint,
        NodeLabel::DbTable,
        NodeLabel::Queue,
    ] {
        let found: Vec<NodeRec> = store
            .nodes_by_label(label, 100_000)?
            .into_iter()
            .filter(|n| n.name.eq_ignore_ascii_case(text) || n.qualified_name.eq_ignore_ascii_case(text))
            .collect();
        if !found.is_empty() {
            return Ok(found);
        }
    }
    Ok(Vec::new())
}
