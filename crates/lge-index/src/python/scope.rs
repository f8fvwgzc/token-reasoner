//! Which names are variables: what a function or a module binds, so that a
//! name passed around can be told apart from a function or class.

use super::strings::text_of;
use super::text;
use crate::facts::Piece;
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

/// The variables visible in a function (its own and those of the functions
/// around it) or at module level.
#[derive(Default, Clone)]
pub(super) struct Bindings {
    pub(super) names: HashSet<String>,
    /// Names assigned exactly once, to a string: the pieces of that string.
    pub(super) strings: HashMap<String, Vec<Piece>>,
}

/// Gathers what one scope binds.
#[derive(Default)]
struct Collector {
    counts: HashMap<String, usize>,
    strings: HashMap<String, Vec<Piece>>,
}

impl Collector {
    fn bind(&mut self, name: &str, value: Option<Vec<Piece>>) {
        let count = self.counts.entry(name.to_string()).or_insert(0);
        *count += 1;
        match value {
            Some(v) if *count == 1 => {
                self.strings.insert(name.to_string(), v);
            }
            // Bound twice, or to something that is not a string: not a constant.
            _ => {
                self.strings.remove(name);
            }
        }
    }

    /// Binds every plain name in an assignment target. `self.x` and `a[0]` bind nothing.
    fn pattern(&mut self, target: Node, src: &[u8]) {
        let mut stack = vec![target];
        while let Some(n) = stack.pop() {
            match n.kind() {
                "identifier" => self.bind(text(n, src), None),
                "pattern_list"
                | "tuple_pattern"
                | "list_pattern"
                | "list_splat_pattern"
                | "dictionary_splat_pattern"
                | "as_pattern_target" => {
                    let mut c = n.walk();
                    stack.extend(n.named_children(&mut c));
                }
                _ => {}
            }
        }
    }

    fn params(&mut self, params: Node, src: &[u8]) {
        let mut c = params.walk();
        for p in params.named_children(&mut c) {
            let name = match p.kind() {
                "default_parameter" | "typed_default_parameter" => p.child_by_field_name("name"),
                "typed_parameter" => p.named_child(0),
                _ => Some(p),
            };
            if let Some(n) = name {
                self.pattern(n, src);
            }
        }
    }

    /// Walks a function body or a module, staying out of nested definitions.
    fn walk(&mut self, start: Node, src: &[u8]) {
        let mut stack = vec![start];
        while let Some(n) = stack.pop() {
            match n.kind() {
                "function_definition" | "class_definition" => continue,
                "assignment" => {
                    if let Some(left) = n.child_by_field_name("left") {
                        if left.kind() == "identifier" {
                            let value = n
                                .child_by_field_name("right")
                                .and_then(|r| text_of(r, src))
                                .map(|t| t.pieces);
                            self.bind(text(left, src), value);
                        } else {
                            self.pattern(left, src);
                        }
                    }
                }
                "augmented_assignment" | "for_statement" | "for_in_clause" => {
                    if let Some(left) = n.child_by_field_name("left") {
                        self.pattern(left, src);
                    }
                }
                "as_pattern" | "except_clause" => {
                    if let Some(alias) = n.child_by_field_name("alias") {
                        self.pattern(alias, src);
                    }
                }
                "named_expression" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        self.pattern(name, src);
                    }
                }
                "lambda" => {
                    if let Some(p) = n.child_by_field_name("parameters") {
                        self.params(p, src);
                    }
                }
                _ => {}
            }
            let mut c = n.walk();
            stack.extend(n.named_children(&mut c));
        }
    }
}

/// Names visible inside `scope_node` (a function definition or the module):
/// its parameters and everything it assigns, on top of `parent`.
pub(super) fn bindings_of(scope_node: Node, src: &[u8], parent: Option<&Bindings>) -> Bindings {
    let mut own = Collector::default();
    if let Some(params) = scope_node.child_by_field_name("parameters") {
        own.params(params, src);
    }
    own.walk(scope_node.child_by_field_name("body").unwrap_or(scope_node), src);
    let mut out = parent.cloned().unwrap_or_default();
    for name in own.counts.into_keys() {
        // A local name hides an outer constant of the same name.
        out.strings.remove(&name);
        out.names.insert(name);
    }
    out.strings.extend(own.strings);
    out
}

pub(super) fn method_names(class_def: Node, src: &[u8]) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(body) = class_def.child_by_field_name("body") else {
        return out;
    };
    let mut c = body.walk();
    for stmt in body.named_children(&mut c) {
        let def = if stmt.kind() == "decorated_definition" {
            stmt.child_by_field_name("definition")
        } else {
            Some(stmt)
        };
        if let Some(name) = def
            .filter(|d| d.kind() == "function_definition")
            .and_then(|d| d.child_by_field_name("name"))
        {
            out.insert(text(name, src).to_string());
        }
    }
    out
}

/// Builtins that are passed around as values (`map(str, xs)`, `@staticmethod`)
/// and never name a symbol of the repository.
const BUILTINS: &[&str] = &[
    "abs",
    "all",
    "any",
    "bool",
    "bytes",
    "callable",
    "classmethod",
    "dict",
    "enumerate",
    "filter",
    "float",
    "frozenset",
    "getattr",
    "hasattr",
    "hash",
    "id",
    "input",
    "int",
    "isinstance",
    "issubclass",
    "iter",
    "len",
    "list",
    "map",
    "max",
    "min",
    "next",
    "object",
    "open",
    "print",
    "property",
    "range",
    "repr",
    "reversed",
    "round",
    "set",
    "setattr",
    "sorted",
    "staticmethod",
    "str",
    "sum",
    "super",
    "tuple",
    "type",
    "zip",
    "BaseException",
    "Exception",
    "AssertionError",
    "AttributeError",
    "ImportError",
    "IndexError",
    "KeyError",
    "KeyboardInterrupt",
    "LookupError",
    "NotImplementedError",
    "OSError",
    "RuntimeError",
    "StopIteration",
    "TimeoutError",
    "TypeError",
    "ValueError",
];

pub(super) fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name) || (name.starts_with("__") && name.ends_with("__"))
}
