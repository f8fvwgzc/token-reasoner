//! Whole-graph views: symbol metrics (rank, community, degree), the graph
//! overview, the architecture summary and the repository map. These are what
//! an agent reads once to orient itself, instead of listing directories.

use crate::algo::{cohesion, louvain, pagerank};
use crate::budget::{count, Budget};
use crate::pack::{commit_line, provenance, symbol_line};
use anyhow::Result;
use lge_core::{Direction, EdgeRec, EdgeType, NodeLabel, NodeRec, PageRec, SymbolMetrics};
use lge_store::GraphStore;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Slug of the generated overview page.
pub const OVERVIEW_SLUG: &str = "graph-overview";
/// Slug of the generated architecture page.
pub const ARCHITECTURE_SLUG: &str = "graph-architecture";
/// Kind of generated pages.
pub const GRAPH_PAGE_KIND: &str = "graph";

/// Communities smaller than this are not listed.
const MIN_COMMUNITY: usize = 3;
const MAX_COMMUNITIES: usize = 12;

/// Rank, community and degree of every symbol that has at least one edge.
///
/// Rank is PageRank where a symbol passes rank to what it calls, references
/// or inherits from, weighted by edge confidence: what much of the code
/// depends on ranks high. Test edges do not count. Communities are Louvain
/// groups over the same edges, ignoring direction.
pub fn compute_metrics(store: &dyn GraphStore) -> Result<(Vec<SymbolMetrics>, Vec<Community>)> {
    let edges = store.symbol_graph()?;
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    for (from, to, _, _) in &edges {
        for id in [from.as_str(), to.as_str()] {
            let next = index.len();
            index.entry(id).or_insert(next);
        }
    }
    // BTreeMap iteration is by id, but indices were handed out in edge order;
    // rebuild them in id order so the result does not depend on edge order.
    let ids: Vec<&str> = index.keys().copied().collect();
    let index: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let n = ids.len();
    let mut links: Vec<(usize, usize, f64)> = Vec::new();
    let mut neighbours: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for (from, to, edge_type, confidence) in &edges {
        let (a, b) = (index[from.as_str()], index[to.as_str()]);
        if a == b || *edge_type == EdgeType::TestedBy {
            continue;
        }
        links.push((a, b, *confidence));
        neighbours[a].insert(b);
        neighbours[b].insert(a);
    }
    links.sort_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)).then(x.2.total_cmp(&y.2)));
    let rank = pagerank(n, &links, 0.85, 60, None);
    let community = louvain(n, &links);

    let metrics: Vec<SymbolMetrics> = (0..n)
        .map(|i| SymbolMetrics {
            id: ids[i].to_string(),
            rank: rank[i],
            community: community[i] as i64,
            degree: neighbours[i].len() as i64,
        })
        .collect();

    let mut members: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for (i, c) in community.iter().enumerate() {
        members.entry(*c).or_default().insert(i);
    }
    let communities: Vec<Community> = members
        .into_iter()
        .filter(|(_, m)| m.len() >= MIN_COMMUNITY)
        .map(|(c, m)| {
            let mut by_rank: Vec<usize> = m.iter().copied().collect();
            by_rank.sort_by(|a, b| rank[*b].total_cmp(&rank[*a]).then(ids[*a].cmp(ids[*b])));
            Community {
                id: c as i64,
                size: m.len(),
                cohesion: cohesion(&m, &links),
                top: by_rank.iter().take(8).map(|i| ids[*i].to_string()).collect(),
            }
        })
        .collect();
    Ok((metrics, communities))
}

/// A group of symbols that are connected more to each other than to the rest.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Community {
    pub id: i64,
    pub size: usize,
    /// Share of member pairs that are directly connected.
    pub cohesion: f64,
    /// Ids of the highest-ranked members, best first.
    pub top: Vec<String>,
}

/// Recompute the metrics, store them, and rebuild the generated pages.
/// Returns the number of symbols that got metrics.
pub fn refresh_graph_views(store: &dyn GraphStore, now: i64) -> Result<usize> {
    let (metrics, communities) = compute_metrics(store)?;
    store.set_metrics(&metrics)?;
    for (slug, title, body) in [
        (OVERVIEW_SLUG, "Graph overview", overview(store, &communities)?),
        (ARCHITECTURE_SLUG, "Architecture", architecture(store, None)?),
    ] {
        store.put_page(&PageRec {
            id: lge_core::stable_id(&["page", slug]),
            slug: slug.to_string(),
            title: title.to_string(),
            body,
            tags: vec!["graph".to_string()],
            kind: GRAPH_PAGE_KIND.to_string(),
            updated_at: now,
            last_memory_seen_at: 0,
        })?;
    }
    Ok(metrics.len())
}

fn stat(stats: &[(String, i64)], key: &str) -> i64 {
    stats.iter().find(|(k, _)| k == key).map(|(_, v)| *v).unwrap_or(0)
}

/// The orientation text: size, services, the symbols most of the code depends
/// on, the communities, and where the graph is unsure.
pub fn overview(store: &dyn GraphStore, communities: &[Community]) -> Result<String> {
    let stats = store.stats()?;
    let commit = store.repository()?.map(|(_, c)| c).unwrap_or_default();
    let mut out = String::from("# Graph overview\n");
    out.push_str(&format!(
        "{} files, {} symbols, {} services, {} endpoints, {} tables, {} queues.\n",
        stat(&stats, "files"),
        stat(&stats, "symbols"),
        stat(&stats, "services"),
        stat(&stats, "endpoints"),
        stat(&stats, "tables"),
        stat(&stats, "queues")
    ));

    let top = store.top_ranked(None, None, 10)?;
    if !top.is_empty() {
        out.push_str("\n## Most depended-on symbols\n");
        let nodes: HashMap<String, NodeRec> = store
            .nodes(&top.iter().map(|h| h.id.clone()).collect::<Vec<_>>())?
            .into_iter()
            .map(|n| (n.id.clone(), n))
            .collect();
        for h in &top {
            if let Some(n) = nodes.get(&h.id) {
                out.push_str(&format!("- {}\n", symbol_line(n, true)));
            }
        }
    }

    let mut listed: Vec<&Community> = communities.iter().collect();
    listed.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));
    listed.truncate(MAX_COMMUNITIES);
    if !listed.is_empty() {
        out.push_str("\n## Communities (symbols that work together)\n");
        let ids: Vec<String> = listed.iter().flat_map(|c| c.top.iter().cloned()).collect();
        let nodes: HashMap<String, NodeRec> = store.nodes(&ids)?.into_iter().map(|n| (n.id.clone(), n)).collect();
        for c in listed {
            let names: Vec<&str> = c
                .top
                .iter()
                .filter_map(|id| nodes.get(id))
                .map(|n| n.qualified_name.as_str())
                .collect();
            let Some(hub) = names.first() else { continue };
            out.push_str(&format!(
                "- c{} around `{}`: {} symbols, cohesion {:.2}. {}\n",
                c.id,
                hub,
                c.size,
                c.cohesion,
                names.join(", ")
            ));
        }
    }

    let unsure: Vec<String> = stats
        .iter()
        .filter(|(k, v)| *v > 0 && (k.ends_with("_inferred") || k.ends_with("_unresolved")))
        .map(|(k, v)| format!("{v} {}", k.replace('_', " ")))
        .collect();
    if !unsure.is_empty() {
        out.push_str("\n## Where the graph is unsure\n");
        out.push_str(&format!(
            "{}. Inferred edges are name matches; unresolved edges have several candidates and no target.\n",
            unsure.join(", ")
        ));
    }
    out.push_str(&commit_line(&commit));
    Ok(out)
}

/// Names of the symbols at the other end of `edges`, with provenance marks,
/// sorted and de-duplicated.
fn ends(edges: &[EdgeRec], take_from: bool, names: &HashMap<String, NodeRec>) -> Vec<String> {
    let mut out: Vec<String> = edges
        .iter()
        .filter_map(|e| {
            let id = if take_from { &e.from } else { &e.to };
            let n = names.get(id)?;
            Some(format!(
                "{}{}",
                if n.qualified_name.is_empty() {
                    &n.name
                } else {
                    &n.qualified_name
                },
                provenance(&e.method, e.confidence)
            ))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Services, their dependencies, and the endpoints, queues and tables that
/// connect them. With `service`, only what that service touches.
pub fn architecture(store: &dyn GraphStore, service: Option<&str>) -> Result<String> {
    let commit = store.repository()?.map(|(_, c)| c).unwrap_or_default();
    let mut services = store.nodes_by_label(NodeLabel::Service, 10_000)?;
    services.sort_by(|a, b| a.name.cmp(&b.name));
    let service_ids: Vec<String> = services.iter().map(|s| s.id.clone()).collect();
    let service_names: HashMap<String, NodeRec> = services.iter().map(|s| (s.id.clone(), s.clone())).collect();
    let depends = store.edges(&service_ids, &[EdgeType::DependsOn], Direction::Out)?;

    let mut out = String::from("# Architecture\n\n## Services\n");
    for s in &services {
        if service.is_some_and(|only| only != s.name) {
            continue;
        }
        let mine: Vec<EdgeRec> = depends.iter().filter(|e| e.from == s.id).cloned().collect();
        let needs = ends(&mine, false, &service_names);
        let needed: Vec<EdgeRec> = depends.iter().filter(|e| e.to == s.id).cloned().collect();
        let needed_by = ends(&needed, true, &service_names);
        out.push_str(&format!(
            "- {}: depends on {}; used by {}\n",
            s.name,
            if needs.is_empty() {
                "nothing".to_string()
            } else {
                needs.join(", ")
            },
            if needed_by.is_empty() {
                "nothing".to_string()
            } else {
                needed_by.join(", ")
            }
        ));
    }

    for (label, title, a_types, a_word, b_types, b_word) in [
        (
            NodeLabel::ApiEndpoint,
            "Endpoints",
            [EdgeType::ExposesApi],
            "handled by",
            [EdgeType::CallsApi],
            "called by",
        ),
        (
            NodeLabel::Queue,
            "Queues",
            [EdgeType::Publishes],
            "published by",
            [EdgeType::Consumes],
            "consumed by",
        ),
        (
            NodeLabel::DbTable,
            "Tables",
            [EdgeType::WritesTo],
            "written by",
            [EdgeType::ReadsFrom],
            "read by",
        ),
    ] {
        let mut nodes = store.nodes_by_label(label, 10_000)?;
        nodes.retain(|n| n.kind != lge_core::kind::UNRESOLVED);
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        if nodes.is_empty() {
            continue;
        }
        let ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
        let a_edges = store.edges(&ids, &a_types, Direction::In)?;
        let b_edges = store.edges(&ids, &b_types, Direction::In)?;
        let symbol_ids: Vec<String> = a_edges
            .iter()
            .chain(&b_edges)
            .map(|e| e.from.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let symbols: HashMap<String, NodeRec> = store
            .nodes(&symbol_ids)?
            .into_iter()
            .map(|n| (n.id.clone(), n))
            .collect();
        let mut lines: Vec<String> = Vec::new();
        for n in &nodes {
            let a: Vec<EdgeRec> = a_edges.iter().filter(|e| e.to == n.id).cloned().collect();
            let b: Vec<EdgeRec> = b_edges.iter().filter(|e| e.to == n.id).cloned().collect();
            if let Some(only) = service {
                let touches = a
                    .iter()
                    .chain(&b)
                    .any(|e| symbols.get(&e.from).is_some_and(|s| s.service == only));
                if !touches && n.service != only {
                    continue;
                }
            }
            let (a_names, b_names) = (ends(&a, true, &symbols), ends(&b, true, &symbols));
            let mut line = format!("- {}", n.name);
            if !a_names.is_empty() {
                line.push_str(&format!("  {a_word} {}", a_names.join(", ")));
            }
            if !b_names.is_empty() {
                line.push_str(&format!("  {b_word} {}", b_names.join(", ")));
            }
            lines.push(line);
        }
        if !lines.is_empty() {
            out.push_str(&format!("\n## {title}\n{}\n", lines.join("\n")));
        }
    }
    out.push_str(&commit_line(&commit));
    Ok(out)
}

/// The highest-ranked symbols as signatures, grouped by file, within a token
/// budget. A fixed-size map of the repository or of one service.
pub fn repo_map(store: &dyn GraphStore, service: Option<&str>, budget_tokens: usize) -> Result<String> {
    let commit = store.repository()?.map(|(_, c)| c).unwrap_or_default();
    let top = store.top_ranked(service, None, 400)?;
    let nodes: HashMap<String, NodeRec> = store
        .nodes(&top.iter().map(|h| h.id.clone()).collect::<Vec<_>>())?
        .into_iter()
        .map(|n| (n.id.clone(), n))
        .collect();
    let footer = commit_line(&commit);
    let mut budget = Budget::new(budget_tokens);
    budget.force(count(&footer) + 12);
    // Files in the order their best symbol ranks; symbols by line inside a file.
    let mut files: Vec<String> = Vec::new();
    let mut by_file: BTreeMap<String, Vec<&NodeRec>> = BTreeMap::new();
    let mut left_out = 0usize;
    for h in &top {
        let Some(n) = nodes.get(&h.id) else { continue };
        let line = format!(
            "  {}  {}",
            n.start_line,
            if n.signature.is_empty() {
                &n.qualified_name
            } else {
                &n.signature
            }
        );
        let new_file = !by_file.contains_key(&n.file_path);
        let cost = count(&line) + 1 + if new_file { count(&n.file_path) + 2 } else { 0 };
        if !budget.take(cost) {
            left_out += 1;
            continue;
        }
        if new_file {
            files.push(n.file_path.clone());
        }
        by_file.entry(n.file_path.clone()).or_default().push(n);
    }
    let mut out = String::from("# Repository map (most depended-on symbols)\n");
    for file in files {
        out.push_str(&format!("{file}:\n"));
        let mut symbols = by_file.remove(&file).unwrap_or_default();
        symbols.sort_by_key(|n| n.start_line);
        for n in symbols {
            out.push_str(&format!(
                "  {}  {}\n",
                n.start_line,
                if n.signature.is_empty() {
                    &n.qualified_name
                } else {
                    &n.signature
                }
            ));
        }
    }
    if top.is_empty() {
        out.push_str("No ranked symbols yet. Run `lge index` first.\n");
    }
    if left_out > 0 {
        out.push_str(&format!("{left_out} more symbol(s) not shown.\n"));
    }
    out.push_str(&footer);
    Ok(out)
}
