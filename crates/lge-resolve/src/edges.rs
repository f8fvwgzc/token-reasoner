//! Collects the derived edges and nodes of one run: one edge per (from, to, type),
//! the provenance bands, and the final sorted output.

use lge_core::{confidence, kind, method, unresolved_id, EdgeRec, EdgeStats, EdgeType, NodeLabel, NodeRec, Resolved};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// How sure an edge is. Only the bands of `lge_core::confidence` exist, each tied
/// to its method, so a rule cannot hand out a `static` edge with a guess's confidence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Prov {
    pub confidence: f64,
    pub method: &'static str,
}

impl Prov {
    pub const SAME_FILE: Prov = Prov {
        confidence: confidence::SAME_FILE,
        method: method::STATIC,
    };
    pub const IMPORTED: Prov = Prov {
        confidence: confidence::IMPORTED,
        method: method::STATIC,
    };
    pub const LITERAL: Prov = Prov {
        confidence: confidence::LITERAL,
        method: method::STATIC,
    };
    pub const INFERRED_HIGH: Prov = Prov {
        confidence: confidence::INFERRED_HIGH,
        method: method::INFERRED,
    };
    pub const INFERRED_BARE: Prov = Prov {
        confidence: confidence::INFERRED_BARE,
        method: method::INFERRED,
    };
    pub const INFERRED_RECEIVER: Prov = Prov {
        confidence: confidence::INFERRED_RECEIVER,
        method: method::INFERRED,
    };
    pub const UNRESOLVED: Prov = Prov {
        confidence: confidence::UNRESOLVED,
        method: method::UNRESOLVED,
    };

    /// A name written out as a literal is static; one assembled at run time is a guess.
    pub fn written(literal: bool) -> Prov {
        if literal {
            Prov::LITERAL
        } else {
            Prov::INFERRED_BARE
        }
    }

    /// The same edge found through a name that was not written as a literal:
    /// it can be at most inferred.
    pub fn unless_literal(self, literal: bool) -> Prov {
        if literal || self.method != method::STATIC {
            self
        } else {
            Prov::INFERRED_BARE
        }
    }
}

/// The source text that justifies an edge.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Evidence<'e> {
    pub file: &'e str,
    pub start: i64,
    pub end: i64,
}

impl<'e> Evidence<'e> {
    pub fn line(file: &'e str, line: i64) -> Self {
        Evidence {
            file,
            start: line,
            end: line,
        }
    }

    pub fn lines(file: &'e str, start: i64, end: i64) -> Self {
        Evidence {
            file,
            start,
            end: end.max(start),
        }
    }

    /// The evidence of an existing edge, for edges derived from it.
    pub fn of(edge: &'e EdgeRec) -> Self {
        Evidence {
            file: &edge.evidence_file,
            start: edge.start_line,
            end: edge.end_line,
        }
    }
}

pub(crate) fn edge(from: &str, to: &str, edge_type: EdgeType, prov: Prov, ev: Evidence) -> EdgeRec {
    EdgeRec {
        from: from.to_string(),
        to: to.to_string(),
        edge_type,
        confidence: prov.confidence,
        method: prov.method.to_string(),
        evidence_file: ev.file.to_string(),
        start_line: ev.start,
        end_line: ev.end,
    }
}

/// static before inferred before unresolved.
pub(crate) fn method_rank(method: &str) -> u8 {
    match method {
        method::STATIC => 0,
        method::INFERRED => 1,
        method::UNRESOLVED => 2,
        _ => 3,
    }
}

/// Which of two edges for the same (from, to, type) is kept; `Less` means `a`.
/// Highest confidence, then the stronger method, then the lowest line. Evidence
/// without a line (line 0, an import) comes after evidence with one. The order
/// is total, so the result does not depend on the order edges were found in.
fn preference(a: &EdgeRec, b: &EdgeRec) -> Ordering {
    b.confidence
        .total_cmp(&a.confidence)
        .then_with(|| method_rank(&a.method).cmp(&method_rank(&b.method)))
        .then_with(|| (a.start_line <= 0).cmp(&(b.start_line <= 0)))
        .then_with(|| a.start_line.cmp(&b.start_line))
        .then_with(|| a.end_line.cmp(&b.end_line))
        .then_with(|| a.evidence_file.cmp(&b.evidence_file))
}

/// Everything the resolver derives, kept sorted and free of duplicates while it grows.
#[derive(Default)]
pub(crate) struct Output {
    edges: BTreeMap<(EdgeType, String, String), EdgeRec>,
    nodes: BTreeMap<(NodeLabel, String), NodeRec>,
    pub stats: EdgeStats,
}

impl Output {
    /// Keep the edge unless it is a self edge or a better one for the same
    /// (from, to, type) is already there.
    pub fn add_edge(&mut self, e: EdgeRec) {
        if e.from == e.to {
            return;
        }
        let key = (e.edge_type, e.from.clone(), e.to.clone());
        match self.edges.get(&key) {
            Some(old) if preference(old, &e) != Ordering::Greater => {}
            _ => {
                self.edges.insert(key, e);
            }
        }
    }

    /// Add a derived node. The first description of an id wins, so callers add
    /// nodes in a fixed order.
    pub fn add_node(&mut self, n: NodeRec) {
        self.nodes.entry((n.label, n.id.clone())).or_insert(n);
    }

    /// Several symbols called `name` could be meant: store an unresolved edge to
    /// the placeholder for the name instead of picking one.
    pub fn add_unresolved_symbol(&mut self, from: &str, name: &str, edge_type: EdgeType, ev: Evidence) {
        let id = unresolved_id(name);
        self.add_node(NodeRec::bare(&id, NodeLabel::Symbol, name, name, kind::UNRESOLVED));
        self.add_edge(edge(from, &id, edge_type, Prov::UNRESOLVED, ev));
    }

    /// The edges kept so far, sorted by type, from and to.
    pub fn edges(&self) -> impl Iterator<Item = &EdgeRec> {
        self.edges.values()
    }

    pub fn edges_of(&self, edge_type: EdgeType) -> impl Iterator<Item = &EdgeRec> {
        self.edges.values().filter(move |e| e.edge_type == edge_type)
    }

    /// Sorted nodes and edges, with the stored edges counted by type and method.
    pub fn finish(mut self) -> Resolved {
        for e in self.edges.values() {
            self.stats.add(e.edge_type, &e.method);
        }
        Resolved {
            nodes: self.nodes.into_values().collect(),
            edges: self.edges.into_values().collect(),
            stats: self.stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls(prov: Prov, line: i64) -> EdgeRec {
        edge("a", "b", EdgeType::Calls, prov, Evidence::line("f.py", line))
    }

    fn only(out: Output) -> EdgeRec {
        let mut edges = out.finish().edges;
        assert_eq!(edges.len(), 1);
        edges.remove(0)
    }

    #[test]
    fn highest_confidence_wins_in_either_order() {
        for pair in [
            [Prov::INFERRED_RECEIVER, Prov::SAME_FILE],
            [Prov::SAME_FILE, Prov::INFERRED_RECEIVER],
        ] {
            let mut out = Output::default();
            out.add_edge(calls(pair[0], if pair[0] == Prov::SAME_FILE { 9 } else { 2 }));
            out.add_edge(calls(pair[1], if pair[1] == Prov::SAME_FILE { 9 } else { 2 }));
            let e = only(out);
            assert_eq!((e.confidence, e.method.as_str(), e.start_line), (0.95, "static", 9));
        }
    }

    #[test]
    fn equal_confidence_prefers_stronger_method_then_lowest_line() {
        let literal_guess = Prov {
            confidence: 0.90,
            method: method::INFERRED,
        };
        for lines in [[7, 3], [3, 7]] {
            let mut out = Output::default();
            out.add_edge(calls(literal_guess, 1));
            out.add_edge(calls(Prov::IMPORTED, lines[0]));
            out.add_edge(calls(Prov::IMPORTED, lines[1]));
            let e = only(out);
            assert_eq!((e.method.as_str(), e.start_line), ("static", 3));
        }
    }

    #[test]
    fn evidence_with_a_line_is_preferred_to_evidence_without_one() {
        for lines in [[0, 12], [12, 0]] {
            let mut out = Output::default();
            out.add_edge(calls(Prov::IMPORTED, lines[0]));
            out.add_edge(calls(Prov::IMPORTED, lines[1]));
            assert_eq!(only(out).start_line, 12);
        }
    }

    #[test]
    fn self_edges_are_dropped_and_types_are_separate() {
        let mut out = Output::default();
        out.add_edge(edge(
            "a",
            "a",
            EdgeType::Calls,
            Prov::SAME_FILE,
            Evidence::line("f.py", 1),
        ));
        out.add_edge(calls(Prov::SAME_FILE, 1));
        out.add_edge(edge(
            "a",
            "b",
            EdgeType::References,
            Prov::SAME_FILE,
            Evidence::line("f.py", 2),
        ));
        let res = out.finish();
        assert_eq!(res.edges.len(), 2);
        assert_eq!(res.stats.get(EdgeType::Calls, "static"), 1);
        assert_eq!(res.stats.get(EdgeType::References, "static"), 1);
    }

    #[test]
    fn unresolved_edge_brings_its_placeholder() {
        let mut out = Output::default();
        out.add_unresolved_symbol("a", "render", EdgeType::Calls, Evidence::line("f.ts", 4));
        out.add_unresolved_symbol("b", "render", EdgeType::Calls, Evidence::line("g.ts", 8));
        let res = out.finish();
        assert_eq!(res.nodes.len(), 1);
        assert_eq!(
            (res.nodes[0].id.as_str(), res.nodes[0].kind.as_str()),
            ("unresolved:render", "unresolved")
        );
        assert_eq!(res.edges.len(), 2);
        assert!(res
            .edges
            .iter()
            .all(|e| e.confidence == 0.0 && e.method == "unresolved"));
    }

    #[test]
    fn a_fact_that_is_not_literal_is_never_static() {
        assert_eq!(Prov::written(true), Prov::LITERAL);
        assert_eq!(Prov::written(false), Prov::INFERRED_BARE);
        assert_eq!(Prov::SAME_FILE.unless_literal(true), Prov::SAME_FILE);
        assert_eq!(Prov::SAME_FILE.unless_literal(false), Prov::INFERRED_BARE);
        assert_eq!(Prov::INFERRED_RECEIVER.unless_literal(false), Prov::INFERRED_RECEIVER);
    }
}
