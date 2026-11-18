//! The evidence pack: what a session needs to know about the code before it
//! starts, in one compact text under a token budget.
//!
//! Order of work: seeds, expansion in both directions, ranking by proximity to
//! the seeds, then filling the budget in rank order. Seeds are never cut; a
//! pack that had to leave things out says how much and what budget shows all.

use crate::algo::pagerank;
use crate::budget::{count, truncate_lines, Budget, DEFAULT_PACK_BUDGET};
use crate::seeds::{find_seeds, Seed, SeedOptions};
use crate::traverse::{dependencies_of, dependents_of, Reached, WalkOptions};
use anyhow::Result;
use lge_core::{fact, method, EdgeRec, Hit, NodeLabel, NodeRec};
use lge_store::GraphStore;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PackOptions {
    pub budget: usize,
    pub max_seeds: usize,
    pub service: Option<String>,
    /// How far to follow what the seeds use.
    pub depth_uses: u8,
    /// How far to follow what uses the seeds (the change impact).
    pub depth_used_by: u8,
    pub max_nodes: usize,
    /// The pack's first line. `None`: `# Context for: <task>`, the task
    /// repeated. A caller that places the pack right after the task's own
    /// text gives a short line here instead; the seeds are still found from
    /// the whole task.
    pub header: Option<String>,
}

impl Default for PackOptions {
    fn default() -> Self {
        PackOptions {
            budget: DEFAULT_PACK_BUDGET,
            max_seeds: 4,
            service: None,
            depth_uses: 1,
            depth_used_by: 2,
            max_nodes: 80,
            header: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct EvidencePack {
    pub task: String,
    /// The text for the model.
    pub text: String,
    pub tokens: usize,
    pub budget: usize,
    /// Indexed commit, "" outside git.
    pub commit: String,
    pub seeds: Vec<Hit>,
    /// Production files of the seeds and of what depends on them.
    pub files: Vec<String>,
    pub services: Vec<String>,
    /// Tests that reach the seeds.
    pub tests: Vec<String>,
    /// Symbols listed in the text, and symbols left out for the budget.
    pub shown: usize,
    pub omitted: usize,
    pub unresolved_edges: i64,
    pub inferred_edges: usize,
}

/// Share of the budget the seed bodies may take together.
const SEED_BODY_SHARE: f64 = 0.5;
/// Neighbours shown with their signature; the rest get one short line.
const FULL_LINES: usize = 12;

fn location(n: &NodeRec) -> String {
    if n.file_path.is_empty() {
        String::new()
    } else if n.end_line > n.start_line {
        format!("{}:{}-{}", n.file_path, n.start_line, n.end_line)
    } else {
        format!("{}:{}", n.file_path, n.start_line)
    }
}

/// `[inferred 0.50]` for anything that is not a verified edge; "" for static.
pub(crate) fn provenance(how: &str, confidence: f64) -> String {
    if how == method::STATIC {
        String::new()
    } else {
        format!(" [{how} {confidence:.2}]")
    }
}

/// One line for a symbol: where it is, what it is, and its signature and
/// the first line of its doc when `full`.
pub(crate) fn symbol_line(n: &NodeRec, full: bool) -> String {
    let mut line = symbol_head(n, full);
    if full && !n.doc.is_empty() {
        line.push_str("  -- ");
        line.push_str(&n.doc);
    }
    line
}

/// [`symbol_line`] without the doc.
fn symbol_head(n: &NodeRec, full: bool) -> String {
    let mut line = format!("{}  {} {}", location(n), n.kind, n.qualified_name);
    if full && !n.signature.is_empty() && n.signature != n.name {
        line.push_str("  ");
        line.push_str(&n.signature);
    }
    line
}

/// The source of a symbol exactly as its file has it: the doc comments and
/// attributes right above it, then its own lines (at most
/// [`lge_index::BODY_CAP`] bytes of them, as much as the index keeps of a
/// body). `None` when the file on disk is not the one that was indexed
/// (changed, deleted, unreadable) or its lines do not hold the symbol's
/// range: the stored body stands in then.
fn exact_source(store: &dyn GraphStore, root: Option<&Path>, n: &NodeRec) -> Option<String> {
    let root = root?;
    if n.file_path.is_empty() || n.start_line < 1 || n.end_line < n.start_line {
        return None;
    }
    let indexed = store.file_info(&n.file_path).ok()??;
    let bytes = std::fs::read(root.join(&n.file_path)).ok()?;
    if lge_core::content_hash(&bytes) != indexed.content_hash {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = (n.start_line as usize - 1, n.end_line as usize);
    if end > lines.len() {
        return None;
    }
    let first = lge_index::leading_start(&n.file_path, &lines, start);
    let mut out = String::new();
    for line in &lines[first..start] {
        out.push_str(line);
        out.push('\n');
    }
    let mut used = 0usize;
    for (i, line) in lines[start..end].iter().enumerate() {
        if i > 0 && used + line.len() > lge_index::BODY_CAP {
            out.push_str("    ... (cut)\n");
            break;
        }
        used += line.len() + 1;
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

/// One line for an endpoint, table or queue.
pub(crate) fn resource_line(n: &NodeRec) -> String {
    match n.label {
        NodeLabel::ApiEndpoint => format!("endpoint {}", n.name),
        NodeLabel::DbTable => format!("table {}", n.name),
        NodeLabel::Queue => format!("{} {}", if n.kind.is_empty() { "queue" } else { &n.kind }, n.name),
        _ => format!("{} {}", n.kind, n.qualified_name),
    }
}

/// Proximity of every walked node to the seeds: personalized PageRank over the
/// walked edges, restarted at the seeds in proportion to their relevance. Rank
/// flows mostly towards what a node depends on and half as much back.
fn proximity(seeds: &[Seed], nodes: &[&NodeRec], edges: &[&EdgeRec]) -> HashMap<String, f64> {
    let index: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
    let mut links: Vec<(usize, usize, f64)> = Vec::new();
    for e in edges {
        let (Some(&a), Some(&b)) = (index.get(e.from.as_str()), index.get(e.to.as_str())) else {
            continue;
        };
        let Some(from_depends) = e.edge_type.from_depends_on_to() else {
            continue;
        };
        let (user, used) = if from_depends { (a, b) } else { (b, a) };
        links.push((user, used, e.confidence));
        links.push((used, user, e.confidence * 0.5));
    }
    let mut restart = vec![0.0; nodes.len()];
    for s in seeds {
        if let Some(&i) = index.get(s.hit.id.as_str()) {
            restart[i] = s.relevance.max(0.05);
        }
    }
    let rank = pagerank(nodes.len(), &links, 0.85, 40, Some(&restart));
    nodes.iter().zip(rank).map(|(n, r)| (n.id.clone(), r)).collect()
}

/// The symbols of a walk that are (or are not) tests, seeds left out, nearest
/// first and then by proximity to the seeds.
fn pick<'a>(
    list: &'a [Reached],
    tests: bool,
    seeds: &HashMap<String, NodeRec>,
    near: &HashMap<String, f64>,
) -> Vec<&'a Reached> {
    let mut v: Vec<&Reached> = list
        .iter()
        .filter(|r| r.node.label == NodeLabel::Symbol && r.node.is_test == tests && !seeds.contains_key(&r.node.id))
        .collect();
    v.sort_by(|a, b| {
        let (ra, rb) = (
            near.get(&a.node.id).copied().unwrap_or(0.0),
            near.get(&b.node.id).copied().unwrap_or(0.0),
        );
        a.hops
            .cmp(&b.hops)
            .then(rb.total_cmp(&ra))
            .then(a.node.qualified_name.cmp(&b.node.qualified_name))
    });
    v
}

/// Build the evidence pack for a task.
pub fn evidence_pack(store: &dyn GraphStore, task: &str, opts: &PackOptions) -> Result<EvidencePack> {
    let (root, commit) = store.repository()?.unwrap_or_default();
    let root = (!root.is_empty()).then(|| PathBuf::from(root));
    let seed_opts = SeedOptions {
        max_seeds: opts.max_seeds,
        service: opts.service.clone(),
        ..SeedOptions::default()
    };
    let seeds = find_seeds(store, task, &seed_opts)?;
    let mut pack = EvidencePack {
        task: task.to_string(),
        budget: opts.budget,
        commit: commit.clone(),
        ..EvidencePack::default()
    };
    if seeds.is_empty() {
        pack.text = format!(
            "# Context for: {task}\nNo symbol in the code graph matches this task. Search with other words, or name a file or symbol.\n{}",
            commit_line(&commit)
        );
        pack.tokens = count(&pack.text);
        return Ok(pack);
    }

    let seed_ids: Vec<String> = seeds.iter().map(|s| s.hit.id.clone()).collect();
    let seed_nodes: HashMap<String, NodeRec> = store.nodes(&seed_ids)?.into_iter().map(|n| (n.id.clone(), n)).collect();
    let uses = dependencies_of(
        store,
        &seed_ids,
        &WalkOptions {
            depth: opts.depth_uses,
            max_nodes: opts.max_nodes,
            ..WalkOptions::default()
        },
    )?;
    let used_by = dependents_of(
        store,
        &seed_ids,
        &WalkOptions {
            depth: opts.depth_used_by,
            max_nodes: opts.max_nodes,
            ..WalkOptions::default()
        },
    )?;

    // Rank everything by proximity to the seeds.
    let mut all_nodes: Vec<&NodeRec> = seed_ids.iter().filter_map(|id| seed_nodes.get(id)).collect();
    all_nodes.extend(uses.reached.iter().chain(&used_by.reached).map(|r| &r.node));
    let all_edges: Vec<&EdgeRec> = uses.edges.iter().chain(&used_by.edges).collect();
    let near = proximity(&seeds, &all_nodes, &all_edges);
    let uses_symbols = pick(&uses.reached, false, &seed_nodes, &near);
    let used_by_symbols = pick(&used_by.reached, false, &seed_nodes, &near);
    let tests = pick(&used_by.reached, true, &seed_nodes, &near);
    let mut resources: Vec<(&'static str, &Reached)> = Vec::new();
    for (role, list) in [("uses", &uses.reached), ("affects", &used_by.reached)] {
        for r in list.iter().filter(|r| r.node.label != NodeLabel::Symbol) {
            resources.push((role, r));
        }
    }

    // What the pack says about the change, whatever the budget shows.
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut services: BTreeSet<String> = BTreeSet::new();
    for n in seed_nodes.values().chain(used_by_symbols.iter().map(|r| &r.node)) {
        if !n.is_test && !n.file_path.is_empty() {
            files.insert(n.file_path.clone());
        }
        if !n.is_test && !n.service.is_empty() {
            services.insert(n.service.clone());
        }
    }
    for r in used_by
        .reached
        .iter()
        .filter(|r| r.node.label != NodeLabel::Symbol && !r.node.service.is_empty())
    {
        services.insert(r.node.service.clone());
    }
    let mut unresolved: Vec<&EdgeRec> = uses.unresolved.iter().chain(&used_by.unresolved).collect();
    unresolved.sort_by(|a, b| (&a.evidence_file, a.start_line, &a.to).cmp(&(&b.evidence_file, b.start_line, &b.to)));
    unresolved.dedup_by(|a, b| a.from == b.from && a.to == b.to && a.edge_type == b.edge_type);
    let mut inferred: Vec<&EdgeRec> = all_edges
        .iter()
        .copied()
        .filter(|e| e.method == method::INFERRED)
        .collect();
    inferred.sort_by(|a, b| (&a.evidence_file, a.start_line, &a.to).cmp(&(&b.evidence_file, b.start_line, &b.to)));
    inferred.dedup_by(|a, b| a.from == b.from && a.to == b.to && a.edge_type == b.edge_type);

    // ---- fill the budget ---------------------------------------------------
    let mut budget = Budget::new(opts.budget);
    let mut text = String::new();
    let mut shown = 0usize;
    let mut omitted = 0usize;
    // Kept back for the closing lines, which must always fit.
    let reserve = 60usize.min(opts.budget / 4);
    budget.force(reserve);
    let emit_forced = |text: &mut String, budget: &mut Budget, line: &str| {
        budget.force(count(line) + 1);
        text.push_str(line);
        text.push('\n');
    };

    let header = opts.header.clone().unwrap_or_else(|| format!("# Context for: {task}"));
    emit_forced(&mut text, &mut budget, &header);
    emit_forced(
        &mut text,
        &mut budget,
        &format!(
            "{} file(s), {} service(s), {} test(s) reach the starting points.",
            files.len(),
            services.len(),
            tests.len()
        ),
    );

    // The conclusions the graph has already worked out, first: the model can
    // start from them instead of deriving them in its reasoning.
    for line in in_short(
        &seed_nodes,
        &seeds,
        &used_by_symbols,
        &tests,
        &resources,
        unresolved.len() + inferred.len(),
    ) {
        emit_forced(&mut text, &mut budget, &line);
    }
    emit_forced(
        &mut text,
        &mut budget,
        "Edges without a mark were resolved from the source and need no re-checking; only those marked inferred or unresolved do.",
    );

    emit_forced(&mut text, &mut budget, "\n## Start here");
    let body_allowance = ((opts.budget as f64 * SEED_BODY_SHARE) as usize / seeds.len()).max(40);
    let rationale = rationale_by_symbol(store, &seed_ids);
    for seed in &seeds {
        let Some(node) = seed_nodes.get(&seed.hit.id) else {
            continue;
        };
        // The line is never cut; its doc comes after the notes and the
        // source, and only when the source is not shown (it has the doc as
        // written, which is what an edit has to match).
        let head = symbol_head(node, true);
        let head_cost = count(&head) + 1;
        budget.force(head_cost);
        shown += 1;
        let mut notes = String::new();
        for why in rationale.get(&node.id).into_iter().flatten().take(3) {
            let line = format!("  note: {why}");
            if budget.take(count(&line) + 1) {
                notes.push_str(&line);
                notes.push('\n');
            }
        }
        // Only a symbol whose source the pack shows gets it from the file: a
        // module has no stored body, and its whole file is not its source.
        let stored = store.symbol_body(&node.id)?.unwrap_or_default();
        let body = match stored.trim().is_empty() {
            true => stored,
            false => exact_source(store, root.as_deref(), node).unwrap_or(stored),
        };
        let mut block = String::new();
        if !body.trim().is_empty() {
            let (cut, tokens) = truncate_lines(
                body.trim_end(),
                body_allowance.min(budget.remaining().saturating_sub(8)),
            );
            // A body cut down to its marker tells nothing.
            if cut.lines().count() > 1 && budget.take(tokens + 8) {
                block = format!("```\n{cut}\n```\n");
            }
        }
        let with_doc = symbol_line(node, true);
        if block.is_empty() && with_doc != head && budget.take((count(&with_doc) + 1).saturating_sub(head_cost)) {
            text.push_str(&with_doc);
        } else {
            text.push_str(&head);
        }
        text.push('\n');
        text.push_str(&notes);
        text.push_str(&block);
    }

    let mut section = |text: &mut String, budget: &mut Budget, title: &str, lines: Vec<String>, count_symbols: bool| {
        if lines.is_empty() {
            return;
        }
        let header = format!("\n## {title}");
        if !budget.take(count(&header) + 1) {
            if count_symbols {
                omitted += lines.len();
            }
            return;
        }
        text.push_str(&header);
        text.push('\n');
        for line in lines {
            if budget.take(count(&line) + 1) {
                text.push_str(&line);
                text.push('\n');
                if count_symbols {
                    shown += 1;
                }
            } else if count_symbols {
                omitted += 1;
            }
        }
    };

    let uses_lines: Vec<String> = uses_symbols
        .iter()
        .enumerate()
        .map(|(i, r)| {
            format!(
                "{}{}",
                symbol_line(&r.node, i < FULL_LINES),
                provenance(&r.method, r.confidence)
            )
        })
        .collect();
    section(
        &mut text,
        &mut budget,
        "Uses (what the starting points call or depend on)",
        uses_lines,
        true,
    );

    let used_by_lines: Vec<String> = used_by_symbols
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let site = if r.hops == 1 && !r.via.evidence_file.is_empty() {
                format!("  at {}:{}", r.via.evidence_file, r.via.start_line)
            } else {
                String::new()
            };
            format!(
                "hop {}  {}{}{}",
                r.hops,
                symbol_line(&r.node, i < FULL_LINES),
                site,
                provenance(&r.method, r.confidence)
            )
        })
        .collect();
    section(
        &mut text,
        &mut budget,
        "Used by (affected if the starting points change)",
        used_by_lines,
        true,
    );

    let test_lines: Vec<String> = tests
        .iter()
        .map(|r| format!("{}{}", symbol_line(&r.node, false), provenance(&r.method, r.confidence)))
        .collect();
    section(&mut text, &mut budget, "Tests to run", test_lines, true);

    let mut resource_lines: Vec<String> = resources
        .iter()
        .map(|(role, r)| {
            format!(
                "{role} {}{}",
                resource_line(&r.node),
                provenance(&r.method, r.confidence)
            )
        })
        .collect();
    resource_lines.sort();
    resource_lines.dedup();
    section(&mut text, &mut budget, "APIs, tables and queues", resource_lines, false);

    let names: HashMap<&str, &str> = all_nodes
        .iter()
        .map(|n| (n.id.as_str(), n.qualified_name.as_str()))
        .collect();
    let name_of = |id: &str| {
        names
            .get(id)
            .copied()
            .unwrap_or_else(|| id.strip_prefix("unresolved:").unwrap_or(id))
            .to_string()
    };
    let mut unsure: Vec<String> = Vec::new();
    for e in &inferred {
        unsure.push(format!(
            "{} -> {}  {} inferred {:.2}  at {}:{}",
            name_of(&e.from),
            name_of(&e.to),
            e.edge_type,
            e.confidence,
            e.evidence_file,
            e.start_line
        ));
    }
    for e in &unresolved {
        unsure.push(format!(
            "{} -> {}  {} unresolved (several candidates)  at {}:{}",
            name_of(&e.from),
            name_of(&e.to),
            e.edge_type,
            e.evidence_file,
            e.start_line
        ));
    }
    section(
        &mut text,
        &mut budget,
        "Check before relying on these (not verified by the graph)",
        unsure,
        false,
    );

    // The closing lines come out of the reserve.
    if omitted > 0 {
        text.push_str(&format!(
            "\n{omitted} more symbol(s) not shown. Ask for a budget of about {} tokens to see all, or query a symbol directly.\n",
            opts.budget * 2
        ));
    }
    text.push_str(&commit_line(&commit));

    pack.tokens = count(&text);
    pack.text = text;
    pack.seeds = seeds.into_iter().map(|s| s.hit).collect();
    pack.files = files.into_iter().collect();
    pack.services = services.into_iter().collect();
    pack.tests = tests.iter().map(|r| r.node.qualified_name.clone()).collect();
    pack.shown = shown;
    pack.omitted = omitted;
    pack.unresolved_edges = unresolved.len() as i64;
    pack.inferred_edges = inferred.len();
    Ok(pack)
}

/// Names of at most `n` nodes, then "and N more".
fn some_names<'a>(names: impl Iterator<Item = &'a str>, n: usize) -> String {
    let all: Vec<&str> = names.collect();
    let mut out = all.iter().take(n).copied().collect::<Vec<_>>().join(", ");
    if all.len() > n {
        out.push_str(&format!(" and {} more", all.len() - n));
    }
    out
}

/// The pack's conclusions in a few sentences: where the change starts, what it
/// affects, which tests cover it, what it reaches outside the code, and how
/// much of that is not verified. Every sentence comes from the graph.
fn in_short(
    seed_nodes: &HashMap<String, NodeRec>,
    seeds: &[Seed],
    used_by: &[&Reached],
    tests: &[&Reached],
    resources: &[(&'static str, &Reached)],
    unverified: usize,
) -> Vec<String> {
    let mut out = vec!["\n## In short".to_string()];
    let starts: Vec<String> = seeds
        .iter()
        .filter_map(|s| seed_nodes.get(&s.hit.id))
        .map(|n| format!("{} ({}:{})", n.qualified_name, n.file_path, n.start_line))
        .collect();
    out.push(format!(
        "- The change starts at {}.",
        some_names(starts.iter().map(String::as_str), 3)
    ));
    // The nearest dependents: callers when there are any, otherwise whatever
    // is closest (code that reaches it through an endpoint or a queue).
    let nearest_hop = used_by.iter().map(|r| r.hops).min().unwrap_or(0);
    let nearest: Vec<&&Reached> = used_by.iter().filter(|r| r.hops == nearest_hop).collect();
    if used_by.is_empty() {
        out.push("- Nothing else in the code calls or depends on it.".to_string());
    } else {
        let how = if nearest_hop == 1 {
            "directly".to_string()
        } else {
            format!("nearest, {nearest_hop} steps away")
        };
        out.push(format!(
            "- Changing it affects {} symbol(s); {how}: {}.",
            used_by.len(),
            some_names(nearest.iter().map(|r| r.node.qualified_name.as_str()), 4)
        ));
    }
    if tests.is_empty() {
        out.push("- No test reaches it.".to_string());
    } else {
        out.push(format!(
            "- Covered by {} test(s): {}.",
            tests.len(),
            some_names(tests.iter().map(|r| r.node.qualified_name.as_str()), 3)
        ));
    }
    let mut outside: Vec<String> = resources.iter().map(|(_, r)| resource_line(&r.node)).collect();
    outside.sort();
    outside.dedup();
    if !outside.is_empty() {
        out.push(format!(
            "- It reaches {}.",
            some_names(outside.iter().map(String::as_str), 4)
        ));
    }
    if unverified > 0 {
        out.push(format!(
            "- {unverified} link(s) below are not verified; check them in the source before relying on them."
        ));
    }
    out
}

/// The line every answer ends with, so the reader knows how fresh it is.
pub fn commit_line(commit: &str) -> String {
    if commit.is_empty() {
        "commit (not a git repository)\n".to_string()
    } else {
        format!("commit {}\n", &commit[..commit.len().min(12)])
    }
}

/// `NOTE:` / `WHY:` comments of the given symbols. A store without the
/// lookup, or without such facts, gives none.
fn rationale_by_symbol(store: &dyn GraphStore, ids: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in store.facts_of(ids, fact::RATIONALE).unwrap_or_default() {
        out.entry(f.symbol_id).or_default().push(f.key);
    }
    out
}
