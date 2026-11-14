//! Call sites: what is called, and on which receiver.

use super::strings::{dotted, expr_text};
use super::{text, Ctx};
use lge_core::{stable_id, CallSite};
use tree_sitter::Node;

/// Longest receiver text kept on a call site.
const RECEIVER_CHARS: usize = 80;

/// `f()`, `x.f()`, `this.f()`, `a?.f()`, `` tag`...` ``.
pub(super) fn record_call(call: Node, ctx: &mut Ctx, caller_id: &str) {
    let Some((name, receiver)) = call.child_by_field_name("function").and_then(|f| callee(f, ctx.src)) else {
        return;
    };
    // `require("./x")` is an import, and is recorded as one.
    if receiver.is_empty() && name == "require" {
        return;
    }
    push(call, &name, receiver, ctx, caller_id);
}

/// `new Foo()` is a bare call to `Foo`; `new ns.Foo()` a call to `Foo` on `ns`.
pub(super) fn record_new(new: Node, ctx: &mut Ctx, caller_id: &str) {
    let Some((name, receiver)) = new.child_by_field_name("constructor").and_then(|c| callee(c, ctx.src)) else {
        return;
    };
    push(new, &name, receiver, ctx, caller_id);
}

fn push(at: Node, name: &str, receiver: String, ctx: &mut Ctx, caller_id: &str) {
    let line = at.start_position().row as i64 + 1;
    let col = at.start_position().column.to_string();
    ctx.out.calls.push(CallSite {
        id: stable_id(&["call", ctx.path, caller_id, name, &line.to_string(), &col]),
        caller_id: caller_id.to_string(),
        file_path: ctx.path.to_string(),
        name: name.to_string(),
        receiver,
        line,
    });
}

/// Called name and receiver of a callee expression. `None` for a callee that
/// has no name: `(cond ? a : b)()`, `handlers[i]()`, `super()`, `import()`.
fn callee(f: Node, src: &[u8]) -> Option<(String, String)> {
    match f.kind() {
        "identifier" => Some((text(f, src).to_string(), String::new())),
        "member_expression" => {
            let property = f.child_by_field_name("property")?;
            if !matches!(property.kind(), "property_identifier" | "private_property_identifier") {
                return None;
            }
            let object = f.child_by_field_name("object")?;
            Some((text(property, src).to_string(), receiver(object, src)))
        }
        // `callback!()`
        "non_null_expression" => callee(f.named_child(0)?, src),
        _ => None,
    }
}

/// The receiver as the resolver wants it: `self` for `this`, `Foo()` for the
/// result of calling or constructing `Foo`, a dotted name without `?.` and `!`,
/// and the source text for anything else.
fn receiver(object: Node, src: &[u8]) -> String {
    let object = if object.kind() == "non_null_expression" {
        object.named_child(0).unwrap_or(object)
    } else {
        object
    };
    let made_by = match object.kind() {
        "this" => return "self".to_string(),
        "new_expression" => object.child_by_field_name("constructor"),
        "call_expression" => object.child_by_field_name("function"),
        _ => None,
    };
    let written = match made_by {
        Some(maker) => dotted(maker, src).map(|name| format!("{name}()")),
        None => dotted(object, src),
    };
    written
        .unwrap_or_else(|| expr_text(object, src))
        .chars()
        .take(RECEIVER_CHARS)
        .collect()
}
