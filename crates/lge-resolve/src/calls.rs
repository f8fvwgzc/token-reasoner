//! Call resolution: every call site becomes a `CALLS` edge with its provenance,
//! an unresolved edge to a placeholder, or a counted external call.

use crate::edges::{edge, Evidence, Output, Prov};
use crate::lookup::{Found, Resolver};
use crate::symbols::Want;
use lge_core::{CallSite, EdgeType, SymbolRec};

/// Method names too common to match by name alone (`x.get()` is usually a dict).
const COMMON_METHODS: &[&str] = &[
    "get",
    "set",
    "add",
    "append",
    "extend",
    "update",
    "items",
    "keys",
    "values",
    "format",
    "join",
    "split",
    "strip",
    "read",
    "write",
    "close",
    "open",
    "pop",
    "copy",
    "count",
    "index",
    "sort",
    "filter",
    "all",
    "first",
    "encode",
    "decode",
    "lower",
    "upper",
    "replace",
    "startswith",
    "endswith",
    "json",
    "dict",
    "run",
    "start",
    "stop",
];

/// On a receiver whose type is unknown, a short or very common method name says
/// nothing about which symbol is meant.
fn too_common(name: &str) -> bool {
    name.len() < 4 || COMMON_METHODS.contains(&name)
}

pub(crate) fn resolve(resolver: &Resolver, calls: &[CallSite], out: &mut Output) {
    for call in calls {
        let Some(caller) = resolver.ix.sym(&call.caller_id) else {
            continue;
        };
        // A call is written in its caller's file; a site without a path says nothing else.
        let file = if call.file_path.is_empty() {
            caller.file_path.as_str()
        } else {
            call.file_path.as_str()
        };
        let evidence = Evidence::line(file, call.line);
        match find(resolver, caller, call, file) {
            Found::Symbol(callee, prov) => {
                out.add_edge(edge(&caller.id, &callee.id, EdgeType::Calls, prov, evidence));
            }
            Found::Ambiguous => out.add_unresolved_symbol(&caller.id, &call.name, EdgeType::Calls, evidence),
            Found::External => out.stats.calls_external += 1,
        }
    }
}

fn find<'a>(resolver: &Resolver<'_, 'a>, caller: &SymbolRec, call: &CallSite, file: &str) -> Found<'a> {
    let (name, receiver) = (call.name.as_str(), call.receiver.as_str());
    let bare = receiver.is_empty();
    let near = if receiver == "self" {
        resolver
            .own_member(caller, name, Want::CallTarget)
            .map(|callee| (callee, Prov::SAME_FILE))
            // Not on the class itself: a method a base class defines.
            .or_else(|| {
                resolver
                    .inherited_member(caller, name, Want::CallTarget)
                    .map(|callee| (callee, Prov::IMPORTED))
            })
    } else if bare {
        resolver.bare(file, name, Want::Callable)
    } else {
        resolver.on_receiver(file, receiver, name, Want::Callable, Want::CallTarget)
    };
    if let Some((callee, prov)) = near {
        return Found::Symbol(callee, prov);
    }
    // Nothing in the source says which symbol is meant: match by name alone.
    if !bare && too_common(name) {
        return Found::External;
    }
    if bare {
        resolver.anywhere(name, Want::Callable, Prov::INFERRED_BARE)
    } else {
        resolver.anywhere(name, Want::Member, Prov::INFERRED_RECEIVER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_and_common_method_names_are_too_common() {
        for name in ["get", "run", "ok", "x", "json", "startswith", "replace", "update"] {
            assert!(too_common(name), "{name}");
        }
        for name in ["save", "save_task", "recordDelivery", "render", "push"] {
            assert!(!too_common(name), "{name}");
        }
    }

    #[test]
    fn the_common_method_list_is_the_original_one() {
        assert_eq!(COMMON_METHODS.len(), 37);
        assert_eq!(COMMON_METHODS.first(), Some(&"get"));
        assert_eq!(COMMON_METHODS.last(), Some(&"stop"));
    }
}
