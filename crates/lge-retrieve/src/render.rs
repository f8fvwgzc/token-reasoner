//! Compact text forms of traversal results, shared by the CLI and the MCP
//! server. Every form is line-oriented, stable for a given graph, bounded by a
//! token budget, and says what it left out.

use crate::budget::{count, Budget};
use crate::pack::{commit_line, provenance, resource_line, symbol_line};
use crate::traverse::{Deps, Impact, Reached, Trace};
use lge_core::{EdgeRec, NodeLabel, NodeRec};

/// Lines are added while they fit; the closing lines always fit.
struct Out {
    text: String,
    budget: Budget,
    dropped: usize,
}

impl Out {
    fn new(budget_tokens: usize, title: &str) -> Out {
        let mut out = Out { text: String::new(), budget: Budget::new(budget_tokens), dropped: 0 };
        // Kept back for the "not shown" notice and the commit line.
        out.budget.force(40usize.min(budget_tokens / 4));
        out.force(title);
        out
    }

    fn force(&mut self, line: &str) {
        self.budget.force(count(line) + 1);
        self.text.push_str(line);
        self.text.push('\n');
    }

    fn line(&mut self, line: &str) {
        if self.budget.take(count(line) + 1) {
            self.text.push_str(line);
            self.text.push('\n');
        } else {
            self.dropped += 1;
        }
    }

    fn section(&mut self, title: &str, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        self.line(&format!("{title} ({})", lines.len()));
        for l in lines {
            self.line(&l);
        }
    }

    fn finish(mut self, commit: &str, limit: usize) -> String {
        if self.dropped > 0 {
            self.text.push_str(&format!(
                "{} line(s) not shown. Ask with a budget of about {} tokens to see all.\n",
                self.dropped,
                limit * 2
            ));
        }
        self.text.push_str(&commit_line(commit));
        self.text
    }
}

fn describe(n: &NodeRec, full: bool) -> String {
    if n.label == NodeLabel::Symbol {
        symbol_line(n, full)
    } else {
        resource_line(n)
    }
}

fn reached_line(r: &Reached, full: bool) -> String {
    format!("hop {}  {}{}", r.hops, describe(&r.node, full), provenance(&r.method, r.confidence))
}

fn site(e: &EdgeRec) -> String {
    if e.evidence_file.is_empty() {
        String::new()
    } else {
        format!("  at {}:{}", e.evidence_file, e.start_line)
    }
}

/// Who is affected by a change, nearest first, tests and resources apart.
pub fn render_impact(i: &Impact, commit: &str, budget_tokens: usize) -> String {
    let targets: Vec<&str> = i.targets.iter().map(|n| if n.qualified_name.is_empty() { n.name.as_str() } else { n.qualified_name.as_str() }).collect();
    let mut out = Out::new(budget_tokens, &format!("# Impact of changing {}", targets.join(", ")));
    out.force(&format!(
        "{} symbol(s) affected in {} file(s), {} service(s); {} test(s).",
        i.affected.len(),
        i.files.len(),
        i.services.len(),
        i.tests.len()
    ));
    if i.unresolved_edges > 0 || i.inferred_paths > 0 {
        out.force(&format!(
            "Not verified: {} unresolved edge(s) near the targets, {} affected symbol(s) reached only by a name match.",
            i.unresolved_edges, i.inferred_paths
        ));
    }
    out.section("## Affected code", i.affected.iter().map(|r| format!("{}{}", reached_line(r, r.hops == 1), if r.hops == 1 { site(&r.via) } else { String::new() })).collect());
    out.section("## Tests to run", i.tests.iter().map(|r| reached_line(r, false)).collect());
    let resources: Vec<String> = i.endpoints.iter().chain(&i.tables).chain(&i.queues).map(|r| reached_line(r, false)).collect();
    out.section("## APIs, tables and queues", resources);
    if !i.services.is_empty() {
        out.line(&format!("services: {}", i.services.join(", ")));
    }
    if i.truncated {
        out.line("The walk stopped at the node limit; more may be affected.");
    }
    out.finish(commit, budget_tokens)
}

/// The execution flow from or to a symbol, one step per line with its call site.
pub fn render_trace(t: &Trace, commit: &str, budget_tokens: usize) -> String {
    let direction = if t.downstream { "what runs after" } else { "what leads to" };
    let mut out = Out::new(budget_tokens, &format!("# Flow: {direction} {}", describe(&t.root, false)));
    if t.steps.is_empty() {
        out.force("No steps found in the graph.");
    }
    for r in &t.steps {
        out.line(&format!("{}{}", reached_line(r, false), site(&r.via)));
    }
    for e in &t.unresolved {
        out.line(&format!("unresolved: {} (several candidates){}", e.to.strip_prefix("unresolved:").unwrap_or(&e.to), site(e)));
    }
    if t.truncated {
        out.line("The walk stopped at the node limit.");
    }
    out.finish(commit, budget_tokens)
}

/// One-hop neighbours in both directions with the edge type and provenance.
pub fn render_deps(d: &Deps, commit: &str, budget_tokens: usize) -> String {
    let mut out = Out::new(budget_tokens, &format!("# Dependencies of {}", describe(&d.node, true)));
    let line = |(e, n): &(EdgeRec, NodeRec)| format!("{}  {}{}{}", e.edge_type, describe(n, false), provenance(&e.method, e.confidence), site(e));
    out.section("## Uses", d.uses.iter().map(line).collect());
    out.section("## Used by", d.used_by.iter().map(line).collect());
    if d.uses.is_empty() && d.used_by.is_empty() {
        out.force("No edges in the graph.");
    }
    out.finish(commit, budget_tokens)
}
