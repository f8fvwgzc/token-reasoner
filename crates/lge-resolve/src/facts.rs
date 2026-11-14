//! Fact sites become the non-call edges and the nodes they point at: API
//! endpoints, database tables, queues, base classes and referenced symbols.

use crate::edges::{edge, Evidence, Output, Prov};
use crate::lookup::{Found, Resolver};
use crate::routes::{http_method, methods_agree, routes_match};
use crate::symbols::{Index, Want};
use lge_core::{
    endpoint_id, fact, kind, normalize_route, queue_id, table_id, EdgeType, FactSite, NodeLabel, NodeRec, SymbolRec,
};
use std::collections::{BTreeMap, BTreeSet};

/// Counted next to the stored `CALLS_API` edges: a call to a URL no service of
/// the repository exposes.
const EXTERNAL: &str = "external";

/// A route some symbol of the repository handles.
struct Endpoint {
    id: String,
    method: String,
    route: String,
    /// The services that have a handler for it. The endpoint id is the method
    /// and the route, so two services exposing `GET /health` share one node;
    /// a URL path cannot say which of them a client calls.
    services: BTreeSet<String>,
}

pub(crate) fn resolve(resolver: &Resolver, facts: &[FactSite], out: &mut Output) {
    let facts = in_fixed_order(facts);
    // Every exposed route must be known before the first client call is matched.
    let endpoints = expose(resolver.ix, &facts, out);
    for f in facts {
        let Some(owner) = owner(resolver.ix, f) else {
            continue;
        };
        if f.key.trim().is_empty() {
            continue;
        }
        match f.kind.as_str() {
            fact::API_CALL => api_call(owner, f, &endpoints, out),
            fact::DB_READ => table(owner, f, &[EdgeType::ReadsFrom], out),
            fact::DB_WRITE => table(owner, f, &[EdgeType::WritesTo], out),
            // A model class stands for its table in both directions.
            fact::TABLE_MODEL => table(owner, f, &[EdgeType::ReadsFrom, EdgeType::WritesTo], out),
            fact::QUEUE_PUBLISH => queue(owner, f, EdgeType::Publishes, out),
            fact::QUEUE_CONSUME => queue(owner, f, EdgeType::Consumes, out),
            fact::INHERITS => named(resolver, owner, f, EdgeType::Inherits, out),
            fact::IMPLEMENTS => named(resolver, owner, f, EdgeType::Implements, out),
            fact::REFERENCE => named(resolver, owner, f, EdgeType::References, out),
            // `api_expose` is handled above; `default_export` is read by import resolution.
            _ => {}
        }
    }
}

/// The symbol a fact belongs to. When the indexer could not name it (a route
/// handler imported from another file) the id matches no symbol; what the fact
/// says still happens in its file, so the file's module symbol owns it. Facts
/// about one particular class or symbol have no such fallback.
fn owner<'a>(ix: &Index<'a>, f: &FactSite) -> Option<&'a SymbolRec> {
    ix.sym(&f.symbol_id).or_else(|| match f.kind.as_str() {
        fact::INHERITS | fact::IMPLEMENTS | fact::TABLE_MODEL | fact::DEFAULT_EXPORT => None,
        _ => ix.module_of(&f.file_path),
    })
}

/// When two facts describe the same node (one table spelled `Orders` and
/// `orders`, one route with two handlers), the first one names it. Sorting makes
/// "first" independent of the order the store returned the facts in.
fn in_fixed_order(facts: &[FactSite]) -> Vec<&FactSite> {
    let mut sorted: Vec<&FactSite> = facts.iter().collect();
    sorted.sort_by(|a, b| {
        (
            &a.file_path,
            a.line,
            a.end_line,
            &a.kind,
            &a.key,
            &a.detail,
            &a.symbol_id,
            a.literal,
            &a.id,
        )
            .cmp(&(
                &b.file_path,
                b.line,
                b.end_line,
                &b.kind,
                &b.key,
                &b.detail,
                &b.symbol_id,
                b.literal,
                &b.id,
            ))
    });
    sorted
}

fn evidence<'e>(owner: &'e SymbolRec, f: &'e FactSite) -> Evidence<'e> {
    let file = if f.file_path.is_empty() {
        &owner.file_path
    } else {
        &f.file_path
    };
    Evidence::lines(file, f.line, f.end_line)
}

/// `api_expose`: one ApiEndpoint node per (method, normalized route) and an
/// `EXPOSES_API` edge from each handler. An empty route is the root.
fn expose(ix: &Index, facts: &[&FactSite], out: &mut Output) -> Vec<Endpoint> {
    let mut endpoints: BTreeMap<String, Endpoint> = BTreeMap::new();
    for f in facts.iter().filter(|f| f.kind == fact::API_EXPOSE) {
        let Some(handler) = owner(ix, f) else { continue };
        let method = http_method(&f.detail);
        let route = normalize_route(&f.key);
        let id = endpoint_id(&method, &route);
        let mut node = NodeRec::bare(
            &id,
            NodeLabel::ApiEndpoint,
            &format!("{method} {route}"),
            &route,
            &method,
        );
        node.file_path = handler.file_path.clone();
        node.service = handler.service.clone();
        node.start_line = f.line;
        node.end_line = f.end_line.max(f.line);
        out.add_node(node);
        out.add_edge(edge(
            &handler.id,
            &id,
            EdgeType::ExposesApi,
            Prov::written(f.literal),
            evidence(handler, f),
        ));
        endpoints
            .entry(id.clone())
            .or_insert_with(|| Endpoint {
                id,
                method,
                route,
                services: BTreeSet::new(),
            })
            .services
            .insert(handler.service.clone());
    }
    endpoints.into_values().collect()
}

/// `api_call`: a URL path that fits an exposed route is a name match, not proof,
/// so the edge is inferred even when the URL is a literal. Several fitting
/// routes, or one route that several services expose, are several candidates:
/// an unresolved edge to a placeholder endpoint.
fn api_call(owner: &SymbolRec, f: &FactSite, endpoints: &[Endpoint], out: &mut Output) {
    let method = http_method(&f.detail);
    let route = normalize_route(&f.key);
    let mut fitting: Vec<&Endpoint> = endpoints
        .iter()
        .filter(|e| methods_agree(&e.method, &method) && routes_match(&e.route, &route))
        .collect();
    // The same path written out beats one that only fits through a parameter.
    if fitting.iter().any(|e| e.route == route) {
        fitting.retain(|e| e.route == route);
    }
    match fitting.as_slice() {
        [] => out.stats.add(EdgeType::CallsApi, EXTERNAL),
        [only] if only.services.len() < 2 => {
            let prov = if f.literal {
                Prov::INFERRED_HIGH
            } else {
                Prov::INFERRED_BARE
            };
            out.add_edge(edge(&owner.id, &only.id, EdgeType::CallsApi, prov, evidence(owner, f)));
        }
        _ => {
            let name = format!("{method} {route}");
            let id = format!("unresolved:{name}");
            out.add_node(NodeRec::bare(
                &id,
                NodeLabel::ApiEndpoint,
                &name,
                &route,
                kind::UNRESOLVED,
            ));
            out.add_edge(edge(
                &owner.id,
                &id,
                EdgeType::CallsApi,
                Prov::UNRESOLVED,
                evidence(owner, f),
            ));
        }
    }
}

/// `db_read`, `db_write`, `table_model`: a DbTable node and an edge per direction.
fn table(owner: &SymbolRec, f: &FactSite, edge_types: &[EdgeType], out: &mut Output) {
    let id = table_id(&f.key);
    out.add_node(NodeRec::bare(&id, NodeLabel::DbTable, &f.key, &f.key, ""));
    for edge_type in edge_types {
        out.add_edge(edge(
            &owner.id,
            &id,
            *edge_type,
            Prov::written(f.literal),
            evidence(owner, f),
        ));
    }
}

/// `queue_publish`, `queue_consume`: a Queue node and one edge.
fn queue(owner: &SymbolRec, f: &FactSite, edge_type: EdgeType, out: &mut Output) {
    let id = queue_id(&f.key);
    let queue_kind = if f.detail.is_empty() {
        "queue"
    } else {
        f.detail.as_str()
    };
    out.add_node(NodeRec::bare(&id, NodeLabel::Queue, &f.key, &f.key, queue_kind));
    out.add_edge(edge(
        &owner.id,
        &id,
        edge_type,
        Prov::written(f.literal),
        evidence(owner, f),
    ));
}

/// `inherits`, `implements`, `reference`: the written name is looked up like a
/// call, against classes and interfaces (any symbol for a reference).
fn named(resolver: &Resolver, owner: &SymbolRec, f: &FactSite, edge_type: EdgeType, out: &mut Output) {
    let Some((prefix, name)) = written_name(&f.key) else {
        return;
    };
    let (near, wide) = if edge_type == EdgeType::References {
        (Want::Nameable, Want::Any)
    } else {
        (Want::Base, Want::Base)
    };
    let file = if f.file_path.is_empty() {
        owner.file_path.as_str()
    } else {
        f.file_path.as_str()
    };
    let hit = match prefix {
        None => resolver.bare(file, name, near),
        Some("self" | "this" | "cls") => resolver.own_member(owner, name, wide).map(|s| (s, Prov::SAME_FILE)),
        Some(receiver) => resolver.on_receiver(file, receiver, name, near, wide),
    };
    let found = match hit {
        Some((target, prov)) => Found::Symbol(target, prov),
        None => resolver.anywhere(name, wide, Prov::INFERRED_BARE),
    };
    match found {
        Found::Symbol(target, prov) => {
            let prov = prov.unless_literal(f.literal);
            out.add_edge(edge(&owner.id, &target.id, edge_type, prov, evidence(owner, f)));
        }
        // An ambiguous base is a dependency the graph must not lose. An ambiguous
        // reference is only a mention, and is not stored.
        Found::Ambiguous if edge_type != EdgeType::References => {
            out.add_unresolved_symbol(&owner.id, name, edge_type, evidence(owner, f));
        }
        Found::Ambiguous | Found::External => {}
    }
}

/// Splits a name as written into its dotted prefix and last segment, without
/// type arguments: `Base`, `models.Base`, `Repository<Order>`, `Generic[T]`.
fn written_name(key: &str) -> Option<(Option<&str>, &str)> {
    let end = key.find(['<', '[', '(']).unwrap_or(key.len());
    let written = key[..end].trim();
    let (prefix, name) = match written.rsplit_once('.') {
        Some((prefix, name)) => (Some(prefix.trim()), name.trim()),
        None => (None, written),
    };
    if name.is_empty() || name.contains(char::is_whitespace) {
        None
    } else {
        Some((prefix, name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_names_lose_type_arguments_and_keep_their_prefix() {
        assert_eq!(written_name("Base"), Some((None, "Base")));
        assert_eq!(written_name(" models.Base "), Some((Some("models"), "Base")));
        assert_eq!(written_name("a.b.Base"), Some((Some("a.b"), "Base")));
        assert_eq!(written_name("Repository<Order>"), Some((None, "Repository")));
        assert_eq!(written_name("Generic[T]"), Some((None, "Generic")));
        assert_eq!(written_name("mixin(Base)"), Some((None, "mixin")));
        assert_eq!(written_name(""), None);
        assert_eq!(written_name("<T>"), None);
        assert_eq!(written_name("models."), None);
        assert_eq!(written_name("not a name"), None);
    }
}
