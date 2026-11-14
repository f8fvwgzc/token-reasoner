//! Which names are variables: what a function or a module binds, so that a
//! name passed around can be told apart from a function or class.

use super::blocks::{test_block, TestKind};
use super::imports::loads_module;
use super::strings::text_of;
use super::{exports_target, is_class_value, is_function_value, text, unwrap};
use crate::facts::Piece;
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

/// The variables visible in a function (its own and those of the functions
/// around it) or at module level. Functions, classes and imported names are not
/// variables: they can name a symbol.
#[derive(Default, Clone)]
pub(super) struct Bindings {
    pub(super) names: HashSet<String>,
    /// `const` names declared exactly once, with a string: the pieces of that string.
    pub(super) strings: HashMap<String, Vec<Piece>>,
}

/// Gathers what one scope binds.
#[derive(Default)]
struct Collector {
    counts: HashMap<String, usize>,
    strings: HashMap<String, Vec<Piece>>,
    /// Functions and classes declared here. They hide an outer variable of the same name.
    definitions: HashSet<String>,
}

impl Collector {
    fn bind(&mut self, name: &str, value: Option<Vec<Piece>>) {
        let count = self.counts.entry(name.to_string()).or_insert(0);
        *count += 1;
        match value {
            Some(v) if *count == 1 => {
                self.strings.insert(name.to_string(), v);
            }
            // Bound twice, or to something that is not a constant string.
            _ => {
                self.strings.remove(name);
            }
        }
    }

    /// Binds every name in a parameter or a destructuring pattern. Default
    /// values are expressions and bind nothing.
    fn pattern(&mut self, target: Node, src: &[u8]) {
        let mut stack = vec![target];
        while let Some(n) = stack.pop() {
            match n.kind() {
                "identifier" | "shorthand_property_identifier_pattern" => self.bind(text(n, src), None),
                "object_pattern" | "array_pattern" | "rest_pattern" => {
                    let mut c = n.walk();
                    stack.extend(n.named_children(&mut c));
                }
                "pair_pattern" => stack.extend(n.child_by_field_name("value")),
                "assignment_pattern" | "object_assignment_pattern" => stack.extend(n.child_by_field_name("left")),
                "required_parameter" | "optional_parameter" => {
                    stack.extend(
                        n.child_by_field_name("pattern")
                            .or_else(|| n.child_by_field_name("name")),
                    );
                }
                _ => {}
            }
        }
    }

    /// Parameters of a function, arrow function or method.
    fn params(&mut self, func: Node, src: &[u8]) {
        // `x => x` has one bare parameter.
        if let Some(single) = func.child_by_field_name("parameter") {
            self.pattern(single, src);
        }
        if let Some(list) = func.child_by_field_name("parameters") {
            let mut c = list.walk();
            for p in list.named_children(&mut c) {
                self.pattern(p, src);
            }
        }
    }

    /// Walks a function body or a module. Definitions that become symbols have
    /// their own scope and are not entered; anonymous callbacks are part of this one.
    fn walk(&mut self, start: Node, src: &[u8]) {
        let mut stack = vec![start];
        while let Some(n) = stack.pop() {
            match n.kind() {
                "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "abstract_class_declaration" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        self.definitions.insert(text(name, src).to_string());
                    }
                    // Its decorators are evaluated out here.
                    stack.extend(decorators_of(n));
                    continue;
                }
                "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "import_statement"
                | "type_annotation"
                | "type_arguments"
                | "type_parameters"
                | "comment" => continue,
                "variable_declarator" => {
                    self.declarator(n, src, &mut stack);
                    continue;
                }
                "arrow_function" | "function_expression" | "generator_function" | "method_definition" => {
                    self.params(n, src);
                    stack.extend(n.child_by_field_name("body"));
                    continue;
                }
                "catch_clause" => {
                    if let Some(p) = n.child_by_field_name("parameter") {
                        self.pattern(p, src);
                    }
                }
                "for_in_statement" => {
                    if let Some(left) = n.child_by_field_name("left") {
                        self.pattern(left, src);
                    }
                }
                "call_expression" => {
                    // The callback of `it("...", () => {})` is a symbol of its own.
                    if let Some(block) = test_block(n, src).filter(|b| b.kind == TestKind::Case) {
                        let mut c = n.walk();
                        for child in n.named_children(&mut c) {
                            let mut c2 = child.walk();
                            stack.extend(child.named_children(&mut c2).filter(|a| a.id() != block.callback.id()));
                        }
                        continue;
                    }
                }
                // `export default () => {}` is a symbol.
                "export_statement" => {
                    let value = n.child_by_field_name("value").map(unwrap);
                    if value.is_some_and(|v| is_function_value(v) || is_class_value(v)) {
                        continue;
                    }
                }
                // So is `exports.f = () => {}`.
                "assignment_expression" => {
                    let exported = n
                        .child_by_field_name("left")
                        .is_some_and(|left| exports_target(left, src).is_some());
                    let value = n.child_by_field_name("right").map(unwrap);
                    if exported && value.is_some_and(|v| is_function_value(v) || is_class_value(v)) {
                        continue;
                    }
                }
                _ => {}
            }
            let mut c = n.walk();
            stack.extend(n.named_children(&mut c));
        }
    }

    fn declarator<'t>(&mut self, declarator: Node<'t>, src: &[u8], stack: &mut Vec<Node<'t>>) {
        let Some(name) = declarator.child_by_field_name("name") else {
            return;
        };
        let value = declarator.child_by_field_name("value");
        let plain = name.kind() == "identifier";
        match value.map(unwrap) {
            Some(v) if plain && (is_function_value(v) || is_class_value(v)) => {
                self.definitions.insert(text(name, src).to_string());
                return;
            }
            // `const x = require("./x")` and `const { a } = require("./x")` are
            // imports. Only what is taken apart further is a variable.
            Some(v) if loads_module(v, src) => {
                let mut c = name.walk();
                for part in name.named_children(&mut c).filter(|_| !plain) {
                    let taken_apart = match part.kind() {
                        "pair_pattern" => part.child_by_field_name("value").filter(|v| v.kind() != "identifier"),
                        "rest_pattern" => Some(part),
                        _ => None,
                    };
                    if let Some(pattern) = taken_apart {
                        self.pattern(pattern, src);
                    }
                }
                return;
            }
            Some(v) if plain && v.kind() == "object" => {
                self.bind(text(name, src), None);
                self.bound_object(v, stack);
                return;
            }
            _ => {}
        }
        if plain {
            let pieces = value
                .filter(|_| is_const(declarator, src))
                .and_then(|v| text_of(v, src))
                .map(|t| t.pieces);
            self.bind(text(name, src), pieces);
        } else {
            self.pattern(name, src);
        }
        stack.extend(value);
    }

    /// An object bound to a name: its functions are symbols, everything else is
    /// ordinary code of this scope.
    fn bound_object<'t>(&mut self, object: Node<'t>, stack: &mut Vec<Node<'t>>) {
        let mut objects = vec![object];
        while let Some(o) = objects.pop() {
            let mut c = o.walk();
            for member in o.named_children(&mut c) {
                match member.kind() {
                    "method_definition" => {}
                    "pair" => match member.child_by_field_name("value").map(unwrap) {
                        Some(v) if is_function_value(v) => {}
                        Some(v) if v.kind() == "object" => objects.push(v),
                        Some(v) => stack.push(v),
                        None => {}
                    },
                    _ => stack.push(member),
                }
            }
        }
    }
}

fn decorators_of(node: Node) -> Vec<Node> {
    let mut c = node.walk();
    let found: Vec<Node> = node
        .named_children(&mut c)
        .filter(|n| n.kind() == "decorator")
        .collect();
    found
}

/// True when the declarator is part of a `const` declaration.
fn is_const(declarator: Node, src: &[u8]) -> bool {
    declarator
        .parent()
        .and_then(|p| p.child_by_field_name("kind"))
        .is_some_and(|k| text(k, src) == "const")
}

impl Bindings {
    /// Names visible inside `scope_node` (a function-like node or the program):
    /// its parameters and everything it declares, on top of `parent`.
    pub(super) fn of(scope_node: Node, src: &[u8], parent: Option<&Bindings>) -> Bindings {
        let mut own = Collector::default();
        own.params(scope_node, src);
        // Default values and parameter decorators can hold callbacks.
        if let Some(parameters) = scope_node.child_by_field_name("parameters") {
            own.walk(parameters, src);
        }
        own.walk(scope_node.child_by_field_name("body").unwrap_or(scope_node), src);
        own.on_top_of(parent)
    }

    /// Names visible in the code a class runs itself: field initializers,
    /// static blocks and members without a name. Methods have their own scope.
    pub(super) fn of_class(class_body: Node, src: &[u8], parent: Option<&Bindings>) -> Bindings {
        let mut own = Collector::default();
        let mut c = class_body.walk();
        for member in class_body.named_children(&mut c) {
            for decorator in decorators_of(member) {
                own.walk(decorator, src);
            }
            match member.kind() {
                // TypeScript writes the decorators of a method in front of it.
                "decorator" => own.walk(member, src),
                "public_field_definition" | "field_definition" => {
                    let value = member.child_by_field_name("value");
                    if let Some(value) = value.filter(|v| !is_function_value(unwrap(*v))) {
                        own.walk(value, src);
                    }
                }
                "method_definition" => {
                    let computed = member
                        .child_by_field_name("name")
                        .is_some_and(|n| n.kind() == "computed_property_name");
                    if computed {
                        own.walk(member, src);
                    }
                }
                "class_static_block" => own.walk(member, src),
                _ => {}
            }
        }
        own.on_top_of(parent)
    }
}

impl Collector {
    /// What this scope binds, added to what the scopes around it bind.
    fn on_top_of(self, parent: Option<&Bindings>) -> Bindings {
        let mut out = parent.cloned().unwrap_or_default();
        for name in &self.definitions {
            out.names.remove(name);
            out.strings.remove(name);
        }
        for name in self.counts.into_keys() {
            // A local name hides an outer constant of the same name.
            out.strings.remove(&name);
            out.names.insert(name);
        }
        out.strings.extend(self.strings);
        out
    }
}

/// True for `get x() {}` and `set x(v) {}`: reading `this.x` gives a value, not the function.
fn is_accessor(method: Node) -> bool {
    let mut c = method.walk();
    let accessor = method.children(&mut c).any(|n| matches!(n.kind(), "get" | "set"));
    accessor
}

/// Names of the members of a class that can be passed around as functions:
/// methods and fields that hold a function.
pub(super) fn member_names(class_body: Node, src: &[u8]) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut c = class_body.walk();
    for member in class_body.named_children(&mut c) {
        let callable = match member.kind() {
            "method_definition" => !is_accessor(member),
            "abstract_method_signature" | "method_signature" => true,
            "public_field_definition" | "field_definition" => member
                .child_by_field_name("value")
                .is_some_and(|v| is_function_value(unwrap(v))),
            _ => false,
        };
        let name = member
            .child_by_field_name("name")
            .or_else(|| member.child_by_field_name("property"));
        if let Some(name) = name.filter(|_| callable) {
            out.insert(text(name, src).to_string());
        }
    }
    out
}

/// Globals that are passed around as values (`map(Number)`, `resolve(undefined)`)
/// and never name a symbol of the repository.
const BUILTINS: &[&str] = &[
    "undefined",
    "NaN",
    "Infinity",
    "globalThis",
    "window",
    "document",
    "navigator",
    "console",
    "process",
    "module",
    "exports",
    "require",
    "arguments",
    "__dirname",
    "__filename",
    "Object",
    "Array",
    "String",
    "Number",
    "Boolean",
    "BigInt",
    "Symbol",
    "Function",
    "Promise",
    "Date",
    "RegExp",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "JSON",
    "Math",
    "Reflect",
    "Proxy",
    "Intl",
    "Buffer",
    "URL",
    "URLSearchParams",
    "AbortController",
    "FormData",
    "Headers",
    "Request",
    "Response",
    "Error",
    "TypeError",
    "RangeError",
    "SyntaxError",
    "ReferenceError",
    "EvalError",
    "AggregateError",
    "parseInt",
    "parseFloat",
    "isNaN",
    "isFinite",
    "encodeURIComponent",
    "decodeURIComponent",
    "encodeURI",
    "decodeURI",
    "setTimeout",
    "clearTimeout",
    "setInterval",
    "clearInterval",
    "setImmediate",
    "queueMicrotask",
    "structuredClone",
    "fetch",
];

pub(super) fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name)
}
