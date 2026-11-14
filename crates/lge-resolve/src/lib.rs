//! Import resolution, call graph, API, database-table and queue edges. Pure: no database access.
//!
//! [`resolve`] takes what the indexer extracted from every file of a repository
//! (symbols, call sites, import sites and fact sites) and returns the derived
//! nodes and edges. Each edge says how it was found:
//!
//! * `static`: the source itself names the target, through the enclosing class,
//!   the same file, an import, or a string literal;
//! * `inferred`: exactly one symbol in the repository has the right name;
//! * `unresolved`: several do. The edge then points at a placeholder node and has
//!   confidence 0. A candidate is never picked.
//!
//! The function reads nothing but its argument and its output is sorted, so the
//! same input gives the same output whatever order its vectors are in.

mod calls;
mod derive;
mod edges;
mod facts;
mod imports;
mod lookup;
mod routes;
mod symbols;

use lge_core::{ResolveInput, Resolved};

/// Resolve a whole repository. Nodes are sorted by label and id, edges by type,
/// `from` and `to`, with at most one edge per (from, to, type).
pub fn resolve(input: &ResolveInput) -> Resolved {
    let ix = symbols::Index::new(input);
    let mut out = edges::Output::default();
    let imports = imports::Imports::resolve(&ix, input, &mut out);
    let resolver = lookup::Resolver {
        ix: &ix,
        imports: &imports,
    };
    calls::resolve(&resolver, &input.calls, &mut out);
    facts::resolve(&resolver, &input.facts, &mut out);
    derive::derive(&ix, &input.facts, &mut out);
    out.finish()
}
