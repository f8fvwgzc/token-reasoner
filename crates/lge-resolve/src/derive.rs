//! Edges that follow from the stored ones: table reads through a model class,
//! `TESTED_BY`, and service-level `DEPENDS_ON`.
//!
//! Unresolved edges derive nothing: a dependency on an unknown target is not a
//! dependency on any particular symbol or service.

use crate::edges::{edge, method_rank, Evidence, Output, Prov};
use crate::symbols::Index;
use lge_core::{fact, method, service_id, table_id, EdgeRec, EdgeType, FactSite};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(crate) fn derive(ix: &Index, facts: &[FactSite], out: &mut Output) {
    // Reads through a model come first: they are table reads like any other and
    // count towards service dependencies.
    add_all(out, model_reads(ix, facts, out));
    add_all(out, tested_by(ix, out));
    add_all(out, depends_on(ix, out));
}

fn add_all(out: &mut Output, edges: Vec<EdgeRec>) {
    for e in edges {
        out.add_edge(e);
    }
}

fn resolved(e: &&EdgeRec) -> bool {
    e.method != method::UNRESOLVED
}

/// A symbol that calls or mentions an ORM model class touches the model's table.
/// Whether it reads or writes is not known, so it is stored as an inferred read.
fn model_reads(ix: &Index, facts: &[FactSite], out: &Output) -> Vec<EdgeRec> {
    let mut tables: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for f in facts
        .iter()
        .filter(|f| f.kind == fact::TABLE_MODEL && !f.key.trim().is_empty())
    {
        if let Some(class) = ix.sym(&f.symbol_id) {
            tables.entry(&class.id).or_default().insert(table_id(&f.key));
        }
    }
    let mut reads = Vec::new();
    for e in out.edges().filter(resolved) {
        if !matches!(e.edge_type, EdgeType::Calls | EdgeType::References) {
            continue;
        }
        for table in tables.get(e.to.as_str()).into_iter().flatten() {
            reads.push(edge(
                &e.from,
                table,
                EdgeType::ReadsFrom,
                Prov::INFERRED_BARE,
                Evidence::of(e),
            ));
        }
    }
    reads
}

/// Symbol -> test, for every call from a test symbol to a symbol that is not a
/// test, with the provenance of that call.
fn tested_by(ix: &Index, out: &Output) -> Vec<EdgeRec> {
    out.edges_of(EdgeType::Calls)
        .filter(resolved)
        .filter_map(|call| {
            let (test, target) = (ix.sym(&call.from)?, ix.sym(&call.to)?);
            (test.is_test && !target.is_test).then(|| EdgeRec {
                from: call.to.clone(),
                to: call.from.clone(),
                edge_type: EdgeType::TestedBy,
                ..call.clone()
            })
        })
        .collect()
}

/// Service A depends on service B when a symbol or file of A has an edge into B,
/// or when both meet at a shared node: A calls an endpoint B exposes, consumes a
/// queue B publishes to, or reads a table B writes.
///
/// Every underlying edge proposes a dependency; the strongest proposal for a pair
/// of services is the one kept.
fn depends_on(ix: &Index, out: &Output) -> Vec<EdgeRec> {
    let symbol_service = |id: &str| ix.sym(id).map(|s| s.service.as_str()).unwrap_or("");
    let file_service = |id: &str| ix.file_by_id(id).map(|f| f.service.as_str()).unwrap_or("");
    let mut deps = Vec::new();
    for e in out.edges().filter(resolved) {
        let (from, to) = match e.edge_type {
            EdgeType::Calls | EdgeType::Inherits | EdgeType::Implements | EdgeType::References => {
                (symbol_service(&e.from), symbol_service(&e.to))
            }
            EdgeType::Imports => (file_service(&e.from), file_service(&e.to)),
            _ => continue,
        };
        deps.extend(dependency(from, to, e, e));
    }
    let meeting_points = [
        (EdgeType::CallsApi, EdgeType::ExposesApi),
        (EdgeType::Consumes, EdgeType::Publishes),
        (EdgeType::ReadsFrom, EdgeType::WritesTo),
    ];
    for (uses, provides) in meeting_points {
        // Per shared node and providing service, the surest providing edge: a
        // weaker one of the same service could only propose a weaker dependency.
        let mut providers: HashMap<&str, BTreeMap<&str, &EdgeRec>> = HashMap::new();
        for p in out.edges_of(provides).filter(resolved) {
            let surest = providers
                .entry(&p.to)
                .or_default()
                .entry(symbol_service(&p.from))
                .or_insert(p);
            if by_strength(p, surest) == Ordering::Greater {
                *surest = p;
            }
        }
        for u in out.edges_of(uses).filter(resolved) {
            for (service, p) in providers.get(u.to.as_str()).into_iter().flatten() {
                deps.extend(dependency(symbol_service(&u.from), service, u, weaker(u, p)));
            }
        }
    }
    deps
}

/// The `DEPENDS_ON` edge between two services, if they are two services.
/// `shown` is the evidence in the dependent service; `strength` says how sure it is.
fn dependency(from: &str, to: &str, shown: &EdgeRec, strength: &EdgeRec) -> Option<EdgeRec> {
    if from.is_empty() || to.is_empty() || from == to {
        return None;
    }
    Some(EdgeRec {
        from: service_id(from),
        to: service_id(to),
        edge_type: EdgeType::DependsOn,
        confidence: strength.confidence,
        method: strength.method.clone(),
        evidence_file: shown.evidence_file.clone(),
        start_line: shown.start_line,
        end_line: shown.end_line,
    })
}

/// `Greater` when `a` is the surer edge: higher confidence, then the stronger method.
fn by_strength(a: &EdgeRec, b: &EdgeRec) -> Ordering {
    a.confidence
        .total_cmp(&b.confidence)
        .then_with(|| method_rank(&b.method).cmp(&method_rank(&a.method)))
}

/// A dependency that needs two edges is only as sure as the less sure of them:
/// a literal route on the server does not make a guessed client call static.
fn weaker<'e>(a: &'e EdgeRec, b: &'e EdgeRec) -> &'e EdgeRec {
    if by_strength(a, b) == Ordering::Greater {
        b
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(prov: Prov) -> EdgeRec {
        edge("a", "b", EdgeType::CallsApi, prov, Evidence::line("f.ts", 1))
    }

    #[test]
    fn the_weaker_of_two_edges_sets_the_strength() {
        let (guess, literal) = (with(Prov::INFERRED_HIGH), with(Prov::LITERAL));
        assert_eq!(weaker(&guess, &literal).method, "inferred");
        assert_eq!(weaker(&literal, &guess).method, "inferred");
        assert_eq!(weaker(&literal, &literal).confidence, 0.90);
        let same_confidence = EdgeRec {
            method: "inferred".into(),
            ..literal.clone()
        };
        assert_eq!(weaker(&literal, &same_confidence).method, "inferred");
        assert_eq!(weaker(&same_confidence, &literal).method, "inferred");
    }

    #[test]
    fn a_service_does_not_depend_on_itself_or_on_nothing() {
        let e = with(Prov::LITERAL);
        assert!(dependency("a", "a", &e, &e).is_none());
        assert!(dependency("", "b", &e, &e).is_none());
        assert!(dependency("a", "", &e, &e).is_none());
        let dep = dependency("a", "b", &e, &e).expect("two services");
        assert_eq!((dep.from, dep.to), (service_id("a"), service_id("b")));
        assert_eq!(dep.edge_type, EdgeType::DependsOn);
    }
}
