//! Definitions: which syntax becomes a symbol, under which name, and which
//! scope its inside is visited in.

use super::comments::leading_doc;
use super::scope::{member_names, Bindings};
use super::strings::dotted;
use super::{
    after_decorators, arguments, callee_name, children_of_kind, collapse, data, decorator_expr, exports_target, http,
    is_class_value, is_function_value, lines, member_name, text, unwrap, visit, visit_children, ClassInfo, Ctx,
    DefaultExport, ExportsTarget, Extent, Scope, MAX_DEPTH,
};
use crate::cap_body;
use lge_core::{fact, kind, symbol_id, SymbolRec};
use tree_sitter::Node;

/// Longest signature kept on a symbol, in chars.
const SIGNATURE_CHARS: usize = 240;

impl Ctx<'_> {
    /// Records a symbol called `name` in `scope`. Returns its id and qualified name.
    fn symbol(
        &mut self,
        scope: &Scope,
        name: &str,
        kind: &'static str,
        extent: Extent,
        signature: String,
        is_test: bool,
    ) -> (String, String) {
        let qn = format!("{}.{name}", scope.qn);
        let id = symbol_id(self.path, &qn, kind);
        let start = extent.first.start_byte();
        let end = extent.last.end_byte().max(start);
        self.out.symbols.push(SymbolRec {
            id: id.clone(),
            name: name.to_string(),
            qualified_name: qn.clone(),
            kind: kind.into(),
            signature,
            doc: leading_doc(extent.first, self.src),
            body: cap_body(&self.src[start..end]),
            file_path: self.path.to_string(),
            service: self.service.to_string(),
            start_line: extent.first.start_position().row as i64 + 1,
            end_line: extent.last.end_position().row as i64 + 1,
            is_test: self.file_is_test || scope.in_test || is_test,
        });
        (id, qn)
    }

    /// The source from `from` to `to` on one line: a declaration up to its body.
    fn signature(&self, from: usize, to: usize) -> String {
        let written = String::from_utf8_lossy(&self.src[from..to.max(from)]);
        collapse(&written)
            .trim_end_matches([';', '{'])
            .trim_end()
            .chars()
            .take(SIGNATURE_CHARS)
            .collect()
    }
}

/// A function-like node that becomes a symbol.
pub(super) struct FunctionDef<'t, 'n> {
    /// The node with the parameters and the body.
    pub(super) func: Node<'t>,
    pub(super) name: &'n str,
    pub(super) kind: &'static str,
    pub(super) extent: Extent<'t>,
    /// Where the signature starts: at `const` for `const f = () => {}`.
    pub(super) sig_start: usize,
    pub(super) is_test: bool,
    /// Reachable by its bare name: a declaration or a variable, not a member.
    pub(super) bare: bool,
}

/// Records the symbol for a function, arrow function or method and visits its
/// inside. Returns the symbol id.
pub(super) fn define_function(def: FunctionDef, ctx: &mut Ctx, scope: &Scope) -> String {
    let body = def.func.child_by_field_name("body");
    let sig_end = body.map_or(def.func.end_byte(), |b| b.start_byte());
    let signature = ctx.signature(def.sig_start, sig_end);
    let (id, qn) = ctx.symbol(scope, def.name, def.kind, def.extent, signature, def.is_test);
    if def.bare {
        ctx.bare_callable.insert(id.clone());
    }
    let locals = Bindings::of(def.func, ctx.src, scope.locals);
    let inner = Scope {
        qn: &qn,
        owner_id: &id,
        caller_id: &id,
        class: scope.class,
        locals: Some(&locals),
        in_test: scope.in_test || def.is_test,
    };
    // Default values and parameter decorators, then the body.
    for part in ["parameters", "body"] {
        if let Some(part) = def.func.child_by_field_name(part) {
            visit(part, ctx, &inner);
        }
    }
    id
}

/// `function f() {}`. `outer` is the declaration with its `export`.
/// Returns the symbol id and name.
pub(super) fn function_declaration(decl: Node, outer: Node, ctx: &mut Ctx, scope: &Scope) -> Option<(String, String)> {
    let name = text(decl.child_by_field_name("name")?, ctx.src).to_string();
    let def = FunctionDef {
        func: decl,
        name: &name,
        kind: kind::FUNCTION,
        extent: Extent::of(outer),
        sig_start: decl.start_byte(),
        is_test: false,
        bare: true,
    };
    let id = define_function(def, ctx, scope);
    Some((id, name))
}

/// `class C {}`. `outer` is the declaration with its `export`, and `decorators`
/// are the ones written before that `export`.
pub(super) fn class_declaration(
    decl: Node,
    outer: Node,
    decorators: &[Node],
    ctx: &mut Ctx,
    scope: &Scope,
) -> Option<(String, String)> {
    let name = text(decl.child_by_field_name("name")?, ctx.src).to_string();
    let id = define_class(
        decl,
        &name,
        Extent::of(outer),
        after_decorators(decl),
        decorators,
        ctx,
        scope,
    );
    Some((id, name))
}

/// Records the symbol for a class, its bases and its decorators, and visits its
/// members. Returns the symbol id.
fn define_class(
    class: Node,
    name: &str,
    extent: Extent,
    sig_start: usize,
    decorators: &[Node],
    ctx: &mut Ctx,
    scope: &Scope,
) -> String {
    let body = class.child_by_field_name("body");
    let sig_end = body.map_or(class.end_byte(), |b| b.start_byte());
    let signature = ctx.signature(sig_start, sig_end);
    let (id, qn) = ctx.symbol(scope, name, kind::CLASS, extent, signature, false);
    heritage(class, &id, ctx, scope);
    let mut decorators = decorators.to_vec();
    decorators.extend(children_of_kind(class, "decorator"));
    let info = ClassInfo {
        id: id.clone(),
        methods: body.map(|b| member_names(b, ctx.src)).unwrap_or_default(),
        prefix: http::controller_prefix(&decorators, ctx, scope),
    };
    for decorator in &decorators {
        decorate(*decorator, &id, true, ctx, scope);
    }
    let Some(body) = body else { return id };
    let locals = Bindings::of_class(body, ctx.src, scope.locals);
    let inner = Scope {
        qn: &qn,
        owner_id: &id,
        caller_id: &id,
        class: Some(&info),
        locals: Some(&locals),
        in_test: scope.in_test,
    };
    class_body(body, ctx, &inner);
    id
}

/// `extends Base` and `implements A, B` of a class.
fn heritage(class: Node, class_id: &str, ctx: &mut Ctx, scope: &Scope) {
    for clause in children_of_kind(class, "class_heritage").into_iter().flat_map(|h| {
        let mut c = h.walk();
        let clauses: Vec<Node> = h.named_children(&mut c).collect();
        clauses
    }) {
        match clause.kind() {
            "extends_clause" => {
                let mut c = clause.walk();
                let bases: Vec<Node> = clause.children_by_field_name("value", &mut c).collect();
                for base in bases {
                    ctx.base(class_id, fact::INHERITS, base, scope);
                }
            }
            "implements_clause" => {
                let mut c = clause.walk();
                let interfaces: Vec<Node> = clause.named_children(&mut c).collect();
                for interface in interfaces {
                    ctx.base(class_id, fact::IMPLEMENTS, interface, scope);
                }
            }
            "comment" => {}
            // JavaScript: the base expression follows `extends` directly.
            _ => ctx.base(class_id, fact::INHERITS, clause, scope),
        }
    }
}

/// The name a base class or an interface is written with, without type arguments.
pub(super) fn type_name(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "generic_type" => dotted(node.child_by_field_name("name")?, src),
        _ => dotted(node, src),
    }
}

/// The members of a class. `scope` is the scope inside the class.
fn class_body(body: Node, ctx: &mut Ctx, scope: &Scope) {
    if ctx.depth >= MAX_DEPTH {
        return;
    }
    ctx.depth += 1;
    // TypeScript writes the decorators of a method as siblings in front of it.
    let mut before: Vec<Node> = Vec::new();
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        match member.kind() {
            "decorator" => {
                before.push(member);
                continue;
            }
            "comment" => continue,
            "method_definition" => method(member, &before, ctx, scope),
            "public_field_definition" | "field_definition" => field(member, &before, ctx, scope),
            "abstract_method_signature" => {
                declared_method(member, ctx, scope);
            }
            // An overload: the implementation that follows is the symbol.
            "method_signature" => {}
            _ => visit(member, ctx, scope),
        }
        before.clear();
    }
    ctx.depth -= 1;
}

/// A method, getter, setter or constructor of a class.
fn method(def: Node, before: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let mut decorators = before.to_vec();
    decorators.extend(children_of_kind(def, "decorator"));
    let Some(name) = member_name(def.child_by_field_name("name"), ctx.src) else {
        // `[Symbol.iterator]() {}` has no name to be found by: its code is the class's.
        for decorator in decorators {
            visit(decorator, ctx, scope);
        }
        for part in ["parameters", "body"] {
            if let Some(part) = def.child_by_field_name(part) {
                visit(part, ctx, scope);
            }
        }
        return;
    };
    let id = symbol_id(ctx.path, &format!("{}.{name}", scope.qn), kind::METHOD);
    for decorator in &decorators {
        decorate(*decorator, &id, false, ctx, scope);
    }
    let def = FunctionDef {
        func: def,
        name: &name,
        kind: kind::METHOD,
        extent: Extent {
            first: decorators.first().copied().unwrap_or(def),
            last: def,
        },
        sig_start: after_decorators(def),
        is_test: false,
        bare: false,
    };
    define_function(def, ctx, scope);
}

/// A class field. One that holds a function is a method: `handle = async () => {}`.
fn field(def: Node, before: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let mut decorators = before.to_vec();
    decorators.extend(children_of_kind(def, "decorator"));
    let name = def
        .child_by_field_name("name")
        .or_else(|| def.child_by_field_name("property"));
    let value = def.child_by_field_name("value");
    let function = value.map(unwrap).filter(|v| is_function_value(*v));
    let (Some(name), Some(function)) = (member_name(name, ctx.src), function) else {
        // A plain field: its decorators and its initializer run in the class.
        for decorator in decorators {
            visit(decorator, ctx, scope);
        }
        if let Some(value) = value {
            visit(value, ctx, scope);
        }
        return;
    };
    let id = symbol_id(ctx.path, &format!("{}.{name}", scope.qn), kind::METHOD);
    for decorator in &decorators {
        decorate(*decorator, &id, false, ctx, scope);
    }
    let def = FunctionDef {
        func: function,
        name: &name,
        kind: kind::METHOD,
        extent: Extent {
            first: decorators.first().copied().unwrap_or(def),
            last: def,
        },
        sig_start: after_decorators(def),
        is_test: false,
        bare: false,
    };
    define_function(def, ctx, scope);
}

/// A method that is declared and not defined: `abstract run(): void` in a
/// class, `send(message: string): void` in an interface or a type.
fn declared_method(signature: Node, ctx: &mut Ctx, scope: &Scope) {
    let Some(name) = member_name(signature.child_by_field_name("name"), ctx.src) else {
        return;
    };
    let written = ctx.signature(signature.start_byte(), signature.end_byte());
    ctx.symbol(scope, &name, kind::METHOD, Extent::of(signature), written, false);
}

/// The method signatures in the body of an interface or an object type, as
/// members of the symbol `qn`.
fn declared_methods(body: Node, qn: &str, ctx: &mut Ctx, scope: &Scope) {
    let inside = Scope { qn, ..*scope };
    for signature in children_of_kind(body, "method_signature") {
        declared_method(signature, ctx, &inside);
    }
}

/// A decorator on the symbol `def_id`. The enclosing scope makes the call; what
/// the decorator says (a route, a table, a guard class) is about the symbol it decorates.
fn decorate(decorator: Node, def_id: &str, on_class: bool, ctx: &mut Ctx, scope: &Scope) {
    let Some(expr) = decorator_expr(decorator) else { return };
    if expr.kind() != "call_expression" {
        // `@Injectable`: the decorated symbol references the decorator.
        let key = dotted(expr, ctx.src).and_then(|name| ctx.reference_key(&name, scope));
        if let Some(key) = key {
            ctx.fact(def_id, fact::REFERENCE, &key, "", lines(expr), true);
        }
        return;
    }
    let inside = Scope {
        owner_id: def_id,
        ..*scope
    };
    visit(expr, ctx, &inside);
    let Some(name) = callee_name(expr, ctx.src) else { return };
    let args = arguments(expr);
    if on_class {
        data::class_decorator(expr, name, &args, def_id, ctx, scope);
    } else {
        http::route_decorator(expr, name, &args, def_id, ctx, scope);
        data::pattern_decorator(expr, name, &args, def_id, ctx, scope);
    }
}

/// `interface I extends J { m(): void }`. Returns the symbol id and name.
pub(super) fn interface(decl: Node, outer: Node, ctx: &mut Ctx, scope: &Scope) -> Option<(String, String)> {
    let name = text(decl.child_by_field_name("name")?, ctx.src).to_string();
    let body = decl.child_by_field_name("body");
    let signature = ctx.signature(decl.start_byte(), body.map_or(decl.end_byte(), |b| b.start_byte()));
    let (id, qn) = ctx.symbol(scope, &name, kind::INTERFACE, Extent::of(outer), signature, false);
    for clause in children_of_kind(decl, "extends_type_clause") {
        let mut c = clause.walk();
        let bases: Vec<Node> = clause.children_by_field_name("type", &mut c).collect();
        for base in bases {
            ctx.base(&id, fact::INHERITS, base, scope);
        }
    }
    if let Some(body) = body {
        declared_methods(body, &qn, ctx, scope);
    }
    Some((id, name))
}

/// `type T = ...`. Returns the symbol id and name.
pub(super) fn type_alias(decl: Node, outer: Node, ctx: &mut Ctx, scope: &Scope) -> Option<(String, String)> {
    let name = text(decl.child_by_field_name("name")?, ctx.src).to_string();
    let signature = ctx.signature(decl.start_byte(), decl.end_byte());
    let (id, qn) = ctx.symbol(scope, &name, kind::TYPE, Extent::of(outer), signature, false);
    if let Some(shape) = decl.child_by_field_name("value").filter(|v| v.kind() == "object_type") {
        declared_methods(shape, &qn, ctx, scope);
    }
    Some((id, name))
}

/// `enum E { A, B }`. Returns the symbol id and name.
pub(super) fn enumeration(decl: Node, outer: Node, ctx: &mut Ctx, scope: &Scope) -> Option<(String, String)> {
    let name = text(decl.child_by_field_name("name")?, ctx.src).to_string();
    let body = decl.child_by_field_name("body");
    let signature = ctx.signature(decl.start_byte(), body.map_or(decl.end_byte(), |b| b.start_byte()));
    let (id, _) = ctx.symbol(scope, &name, kind::ENUM, Extent::of(outer), signature, false);
    // Member values are expressions and can call.
    if let Some(body) = body {
        visit_children(body, ctx, scope);
    }
    Some((id, name))
}

/// `namespace NS { ... }`: no symbol of its own, but its name is part of the
/// qualified names inside it.
pub(super) fn namespace(node: Node, ctx: &mut Ctx, scope: &Scope) {
    let name = node.child_by_field_name("name").and_then(|n| dotted(n, ctx.src));
    match (name, node.child_by_field_name("body")) {
        (Some(name), Some(body)) => {
            let qn = format!("{}.{name}", scope.qn);
            let inside = Scope { qn: &qn, ..*scope };
            visit_children(body, ctx, &inside);
        }
        // `declare module "pkg" { ... }` describes another module.
        _ => visit_children(node, ctx, scope),
    }
}

/// `const a = ..., b = ...`. `outer` is the declaration with its `export`. A
/// variable bound to a function or a class is that symbol; an object literal
/// gives its functions the variable's name as a prefix.
pub(super) fn variables(decl: Node, outer: Node, ctx: &mut Ctx, scope: &Scope) {
    let declarators = children_of_kind(decl, "variable_declarator");
    let alone = declarators.len() == 1;
    for declarator in declarators {
        let name = declarator
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier")
            .map(|n| text(n, ctx.src).to_string());
        let value = declarator.child_by_field_name("value").map(unwrap);
        // One declarator stands for the whole statement, `const` and `export` included.
        let (extent, sig_start) = if alone {
            (Extent::of(outer), decl.start_byte())
        } else {
            (Extent::of(declarator), declarator.start_byte())
        };
        match (name, value) {
            (Some(name), Some(value)) if is_function_value(value) => {
                let def = FunctionDef {
                    func: value,
                    name: &name,
                    kind: kind::FUNCTION,
                    extent,
                    sig_start,
                    is_test: false,
                    bare: true,
                };
                define_function(def, ctx, scope);
            }
            (Some(name), Some(value)) if is_class_value(value) => {
                define_class(value, &name, extent, sig_start, &[], ctx, scope);
            }
            (Some(name), Some(value)) if value.kind() == "object" => {
                object_members(value, &format!("{}.{name}", scope.qn), ctx, scope);
            }
            _ => visit_children(declarator, ctx, scope),
        }
    }
}

/// Members of an object literal bound to a name: its functions are symbols
/// under `qn`, and everything else is code of the enclosing scope.
fn object_members(object: Node, qn: &str, ctx: &mut Ctx, scope: &Scope) {
    if ctx.depth >= MAX_DEPTH {
        return;
    }
    ctx.depth += 1;
    let inside = Scope { qn, ..*scope };
    let mut cursor = object.walk();
    for member in object.named_children(&mut cursor) {
        let (key, value) = match member.kind() {
            "method_definition" => (member_name(member.child_by_field_name("name"), ctx.src), Some(member)),
            "pair" => (
                member_name(member.child_by_field_name("key"), ctx.src),
                member.child_by_field_name("value").map(unwrap),
            ),
            _ => (None, None),
        };
        match (key, value) {
            (Some(key), Some(value)) if is_function_value(value) || value.kind() == "method_definition" => {
                let def = FunctionDef {
                    func: value,
                    name: &key,
                    kind: kind::FUNCTION,
                    extent: Extent::of(member),
                    sig_start: member.start_byte(),
                    is_test: false,
                    bare: false,
                };
                define_function(def, ctx, &inside);
            }
            (Some(key), Some(value)) if value.kind() == "object" => {
                object_members(value, &format!("{qn}.{key}"), ctx, scope);
            }
            _ => visit(member, ctx, scope),
        }
    }
    ctx.depth -= 1;
}

/// A function or class written without a declaration of its own:
/// `export default () => {}`, `module.exports = class {}`. Its own name is used
/// when it has one, `fallback` otherwise. Returns the symbol id and name.
fn define_value(value: Node, fallback: &str, outer: Node, ctx: &mut Ctx, scope: &Scope) -> Option<(String, String)> {
    let own = value.child_by_field_name("name").map(|n| text(n, ctx.src));
    let name = own.unwrap_or(fallback).to_string();
    let id = if is_function_value(value) {
        let def = FunctionDef {
            func: value,
            name: &name,
            kind: kind::FUNCTION,
            extent: Extent::of(outer),
            sig_start: value.start_byte(),
            is_test: false,
            bare: false,
        };
        define_function(def, ctx, scope)
    } else if is_class_value(value) {
        define_class(
            value,
            &name,
            Extent::of(outer),
            after_decorators(value),
            &[],
            ctx,
            scope,
        )
    } else {
        return None;
    };
    Some((id, name))
}

/// `exports.name = ...` and `module.exports = ...` export like `export` does;
/// any other assignment is ordinary code.
pub(super) fn assignment(node: Node, ctx: &mut Ctx, scope: &Scope) {
    let target = node
        .child_by_field_name("left")
        .and_then(|left| exports_target(left, ctx.src));
    let (Some(target), Some(value)) = (target, node.child_by_field_name("right").map(unwrap)) else {
        visit_children(node, ctx, scope);
        return;
    };
    let line = lines(node).0;
    match target {
        ExportsTarget::Named(name) if is_function_value(value) || is_class_value(value) => {
            // The exported name is the one other files use, whatever the function calls itself.
            if is_function_value(value) {
                let def = FunctionDef {
                    func: value,
                    name: &name,
                    kind: kind::FUNCTION,
                    extent: Extent::of(node),
                    sig_start: node.start_byte(),
                    is_test: false,
                    bare: false,
                };
                define_function(def, ctx, scope);
            } else {
                define_class(value, &name, Extent::of(node), node.start_byte(), &[], ctx, scope);
            }
        }
        ExportsTarget::Whole if value.kind() == "identifier" => {
            ctx.default_exports.push(DefaultExport {
                name: text(value, ctx.src).to_string(),
                scope_qn: scope.qn.to_string(),
                line,
            });
        }
        ExportsTarget::Whole if value.kind() == "object" => object_members(value, scope.qn, ctx, scope),
        ExportsTarget::Whole => match define_value(value, "default", node, ctx, scope) {
            Some((id, name)) => ctx.fact(&id, fact::DEFAULT_EXPORT, &name, "", (line, line), true),
            None => visit_children(node, ctx, scope),
        },
        ExportsTarget::Named(_) => visit_children(node, ctx, scope),
    }
}

/// `export ...`. A re-export (`export { a } from "./b"`) is an import site and
/// was read by the prescan.
pub(super) fn export(node: Node, ctx: &mut Ctx, scope: &Scope) {
    if node.child_by_field_name("source").is_some() {
        return;
    }
    let decorators = children_of_kind(node, "decorator");
    let mut c = node.walk();
    let is_default = node.children(&mut c).any(|n| n.kind() == "default");
    let line = lines(node).0;
    let mut exported = None;
    if let Some(decl) = node.child_by_field_name("declaration") {
        match decl.kind() {
            "function_declaration" | "generator_function_declaration" => {
                exported = function_declaration(decl, node, ctx, scope);
            }
            "class_declaration" | "abstract_class_declaration" => {
                exported = class_declaration(decl, node, &decorators, ctx, scope);
            }
            "interface_declaration" => exported = interface(decl, node, ctx, scope),
            "type_alias_declaration" => exported = type_alias(decl, node, ctx, scope),
            "enum_declaration" => exported = enumeration(decl, node, ctx, scope),
            "lexical_declaration" | "variable_declaration" => variables(decl, node, ctx, scope),
            _ => visit(decl, ctx, scope),
        }
    } else if let Some(written) = node.child_by_field_name("value") {
        let value = unwrap(written);
        match value.kind() {
            // `export default handler;`
            "identifier" => ctx.default_exports.push(DefaultExport {
                name: text(value, ctx.src).to_string(),
                scope_qn: scope.qn.to_string(),
                line,
            }),
            "object" => object_members(value, scope.qn, ctx, scope),
            _ => {
                exported = define_value(value, "default", node, ctx, scope);
                if exported.is_none() {
                    visit(written, ctx, scope);
                }
            }
        }
    } else {
        // `export { f as default };` and TypeScript's `export = f;`
        let mut c = node.walk();
        let assigned = node.children(&mut c).any(|n| n.kind() == "=");
        let mut names: Vec<String> = Vec::new();
        if assigned {
            names.extend(
                children_of_kind(node, "identifier")
                    .first()
                    .map(|n| text(*n, ctx.src).to_string()),
            );
        }
        for specifier in children_of_kind(node, "export_clause")
            .into_iter()
            .flat_map(|clause| children_of_kind(clause, "export_specifier"))
        {
            let alias = specifier.child_by_field_name("alias").map(|a| text(a, ctx.src));
            if let (Some("default"), Some(name)) = (alias, specifier.child_by_field_name("name")) {
                names.push(text(name, ctx.src).to_string());
            }
        }
        for name in names {
            ctx.default_exports.push(DefaultExport {
                name,
                scope_qn: scope.qn.to_string(),
                line,
            });
        }
    }
    if let (true, Some((id, name))) = (is_default, exported) {
        ctx.fact(&id, fact::DEFAULT_EXPORT, &name, "", (line, line), true);
    }
}
