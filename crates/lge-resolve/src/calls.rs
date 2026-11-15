//! Call resolution: every call site becomes a `CALLS` edge with its provenance,
//! an unresolved edge to a placeholder, or a counted external call.

use crate::edges::{edge, Evidence, Output, Prov};
use crate::lookup::{Found, Resolver};
use crate::symbols::{Lang, Want};
use lge_core::{kind, lang, CallSite, EdgeType, SymbolRec};

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
    let other = resolver.ix.lang(file) == Lang::Other;
    // In a language where a method calls its siblings without `this`, a
    // member of the enclosing class comes before anything else in the file.
    let implicit = other && IMPLICIT_THIS.contains(&resolver.ix.lang_name(file));
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
    } else if bare && other {
        bare_call(resolver, caller, file, name, implicit)
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
    if !bare {
        return resolver.anywhere(file, name, Want::Member, Prov::INFERRED_RECEIVER);
    }
    let found = resolver.anywhere(file, name, Want::Callable, Prov::INFERRED_BARE);
    // Inside a type of a language with an implicit `this`, a bare name can be a
    // method inherited from a class of another file, or the other half of a
    // partial class: matched by name like `this.name()` would be.
    if matches!(found, Found::External) && implicit && resolver.in_type(caller) && !too_common(name) {
        return resolver.anywhere(file, name, Want::Member, Prov::INFERRED_BARE);
    }
    found
}

/// Languages in which `f()` inside a method can call a method of its class
/// with no receiver written. In Python, TypeScript, JavaScript, Rust, Go, Lua
/// and the others a bare name never reaches a method. R counts for its
/// reference classes, whose methods call each other by name.
const IMPLICIT_THIS: [&str; 12] = [
    lang::JAVA,
    lang::KOTLIN,
    lang::SCALA,
    lang::GROOVY,
    lang::CSHARP,
    lang::CPP,
    lang::CUDA,
    lang::SWIFT,
    lang::DART,
    lang::RUBY,
    lang::SOLIDITY,
    lang::R,
];

/// A bare `name()` in a language read through a tags query. With an implicit
/// `this`, a member of the enclosing type (or of a base the source resolves)
/// is what the source names; then the one function or class of the file in
/// scope at the call.
fn bare_call<'a>(
    resolver: &Resolver<'_, 'a>,
    caller: &SymbolRec,
    file: &str,
    name: &str,
    implicit: bool,
) -> Option<(&'a SymbolRec, Prov)> {
    if implicit {
        if let Some(callee) = resolver.implicit_member(caller, name, Want::CallTarget) {
            return Some((callee, Prov::SAME_FILE));
        }
        if let Some(callee) = resolver.inherited_member(caller, name, Want::CallTarget) {
            return Some((callee, Prov::IMPORTED));
        }
        if let Some(callee) = resolver.outer_member(caller, name, Want::CallTarget) {
            return Some((callee, Prov::INFERRED_BARE));
        }
    }
    let callee = resolver.visible(caller, file, name, Want::Callable)?;
    // A base the source does not resolve may define `name`, and its member
    // comes before a function of the file (C++ looks in the class first).
    if implicit && callee.kind == kind::FUNCTION && resolver.has_unknown_base(caller) {
        return None;
    }
    Some((callee, Prov::SAME_FILE))
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
