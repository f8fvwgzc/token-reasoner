//! Import sites, and the one pass over the file that runs before the visitor:
//! it reads every import and notes which variables hold a router, an HTTP
//! client, a channel or a queue.

use super::strings::{dotted, plain_string};
use super::{text, unwrap, Ctx};
use lge_core::{stable_id, ImportSite};
use tree_sitter::Node;

/// Reads imports and role-giving bindings anywhere in the file, in source order.
pub(super) fn prescan(root: Node, ctx: &mut Ctx) {
    // Bindings are judged after the walk, when every import is known.
    let mut bindings: Vec<(String, Node)> = Vec::new();
    let mut cursor = root.walk();
    'walk: loop {
        let node = cursor.node();
        let descend = match node.kind() {
            "import_statement" => {
                import_statement(node, ctx);
                false
            }
            "export_statement" if node.child_by_field_name("source").is_some() => {
                reexport(node, ctx);
                false
            }
            "variable_declarator" => {
                declarator(node, ctx, &mut bindings);
                true
            }
            "assignment_expression" => {
                let name = node.child_by_field_name("left").and_then(|l| dotted(l, ctx.src));
                if let (Some(name), Some(value)) = (name, node.child_by_field_name("right")) {
                    bindings.push((name, value));
                }
                true
            }
            // A class field is reached as `this.name`.
            "public_field_definition" | "field_definition" => {
                let name = node
                    .child_by_field_name("name")
                    .or_else(|| node.child_by_field_name("property"));
                if let Some(name) = name {
                    let reached = format!("this.{}", text(name, ctx.src));
                    ctx.note_type(&reached, node.child_by_field_name("type"));
                    if let Some(value) = node.child_by_field_name("value") {
                        bindings.push((reached, value));
                    }
                }
                true
            }
            // `constructor(private readonly client: ClientProxy)`
            "required_parameter" | "optional_parameter" => {
                if let Some(name) = node.child_by_field_name("pattern").filter(|p| p.kind() == "identifier") {
                    let name = text(name, ctx.src).to_string();
                    let declared = node.child_by_field_name("type");
                    ctx.note_type(&format!("this.{name}"), declared);
                    ctx.note_type(&name, declared);
                }
                true
            }
            "comment" | "string" | "type_annotation" | "type_arguments" | "type_parameters" => false,
            _ => true,
        };
        if descend && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    for (name, value) in bindings {
        ctx.note_binding(&name, value);
    }
}

impl Ctx<'_> {
    /// Records an import site. `binds` is false for a re-export, which gives the
    /// file no local name.
    fn import(&mut self, module: &str, name: &str, alias: &str, binds: bool) {
        if module.is_empty() {
            return;
        }
        if binds && !alias.is_empty() {
            self.import_sources
                .insert(alias.to_string(), (module.to_string(), name.to_string()));
        }
        self.out.imports.push(ImportSite {
            id: stable_id(&["import", self.path, module, name, alias]),
            file_path: self.path.to_string(),
            module: module.to_string(),
            name: name.to_string(),
            alias: alias.to_string(),
        });
    }
}

/// A name in an import or export specifier: an identifier, or a string
/// (`import { "a-b" as c }`).
fn specifier_name(node: Node, src: &[u8]) -> String {
    plain_string(node, src).unwrap_or_else(|| text(node, src).to_string())
}

fn source_of(node: Node, src: &[u8]) -> Option<String> {
    node.child_by_field_name("source").and_then(|s| plain_string(s, src))
}

/// `import d from`, `import { a as b } from`, `import * as ns from`,
/// `import x = require()`. An import for its side effects binds nothing and is skipped.
fn import_statement(node: Node, ctx: &mut Ctx) {
    let mut c = node.walk();
    let clauses: Vec<Node> = node.named_children(&mut c).collect();
    for clause in clauses {
        match clause.kind() {
            "import_require_clause" => {
                let mut c2 = clause.walk();
                let alias = clause.named_children(&mut c2).find(|n| n.kind() == "identifier");
                if let (Some(alias), Some(module)) = (alias, source_of(clause, ctx.src)) {
                    ctx.import(&module, "*", text(alias, ctx.src), true);
                }
            }
            "import_clause" => {
                let Some(module) = source_of(node, ctx.src) else { return };
                let mut c2 = clause.walk();
                let parts: Vec<Node> = clause.named_children(&mut c2).collect();
                for part in parts {
                    match part.kind() {
                        "identifier" => ctx.import(&module, "default", text(part, ctx.src), true),
                        "namespace_import" => {
                            let mut c3 = part.walk();
                            let alias = part.named_children(&mut c3).find(|n| n.kind() == "identifier");
                            if let Some(alias) = alias {
                                ctx.import(&module, "*", text(alias, ctx.src), true);
                            }
                        }
                        "named_imports" => specifiers(part, &module, true, ctx),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// `{ a, b as c }` of an import or a re-export.
fn specifiers(list: Node, module: &str, binds: bool, ctx: &mut Ctx) {
    let mut c = list.walk();
    let items: Vec<Node> = list
        .named_children(&mut c)
        .filter(|s| matches!(s.kind(), "import_specifier" | "export_specifier"))
        .collect();
    for item in items {
        let Some(name) = item.child_by_field_name("name") else {
            continue;
        };
        let name = specifier_name(name, ctx.src);
        let alias = item
            .child_by_field_name("alias")
            .map(|a| specifier_name(a, ctx.src))
            .unwrap_or_else(|| name.clone());
        ctx.import(module, &name, &alias, binds);
    }
}

/// `export { a as b } from "./x"` imports `a`; `export * as ns from "./x"` the
/// whole module; `export * from "./x"` the whole module under no name.
fn reexport(node: Node, ctx: &mut Ctx) {
    let Some(module) = source_of(node, ctx.src) else { return };
    let mut c = node.walk();
    let clauses: Vec<Node> = node
        .named_children(&mut c)
        .filter(|n| matches!(n.kind(), "export_clause" | "namespace_export"))
        .collect();
    if clauses.is_empty() {
        ctx.import(&module, "*", "", false);
    }
    for clause in clauses {
        if clause.kind() == "export_clause" {
            specifiers(clause, &module, false, ctx);
            continue;
        }
        let mut c2 = clause.walk();
        let alias = clause.named_children(&mut c2).find(|n| n.kind() != "comment");
        if let Some(alias) = alias {
            let alias = specifier_name(alias, ctx.src);
            ctx.import(&module, "*", &alias, false);
        }
    }
}

/// The module that `require("./x")` or `import("./x")` loads, when `call` is one of them.
fn loaded_by(call: Node, src: &[u8]) -> Option<String> {
    if call.kind() != "call_expression" {
        return None;
    }
    let f = call.child_by_field_name("function")?;
    let loads = f.kind() == "import" || (f.kind() == "identifier" && text(f, src) == "require");
    if !loads {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let mut c = args.walk();
    let first = args.named_children(&mut c).find(|n| n.kind() != "comment")?;
    plain_string(first, src)
}

/// The module an expression loads and the member taken from it:
/// `require("./x")`, `await import("./x")`, `require("./x").helper`.
fn loaded_module(value: Node, src: &[u8]) -> Option<(String, Option<String>)> {
    let value = unwrap(value);
    if value.kind() == "member_expression" {
        let module = loaded_by(unwrap(value.child_by_field_name("object")?), src)?;
        let member = value
            .child_by_field_name("property")
            .filter(|p| p.kind() == "property_identifier")?;
        return Some((module, Some(text(member, src).to_string())));
    }
    loaded_by(value, src).map(|module| (module, None))
}

/// True when the expression is a `require` or a dynamic `import`.
pub(super) fn loads_module(value: Node, src: &[u8]) -> bool {
    loaded_module(value, src).is_some()
}

/// `const x = require("./x")`, `const { a, b: c } = require("./x")`,
/// `const m = await import("./x")`. Any other declarator may give its name a role.
fn declarator<'t>(node: Node<'t>, ctx: &mut Ctx, bindings: &mut Vec<(String, Node<'t>)>) {
    let (Some(name), Some(value)) = (node.child_by_field_name("name"), node.child_by_field_name("value")) else {
        return;
    };
    let Some((module, member)) = loaded_module(value, ctx.src) else {
        if name.kind() == "identifier" {
            bindings.push((text(name, ctx.src).to_string(), value));
        }
        return;
    };
    match (name.kind(), member) {
        ("identifier", Some(member)) => ctx.import(&module, &member, text(name, ctx.src), true),
        ("identifier", None) => ctx.import(&module, "*", text(name, ctx.src), true),
        ("object_pattern", None) => {
            let mut c = name.walk();
            let properties: Vec<Node> = name.named_children(&mut c).collect();
            for property in properties {
                let (imported, local) = match property.kind() {
                    "shorthand_property_identifier_pattern" => (Some(property), Some(property)),
                    "pair_pattern" => (
                        property.child_by_field_name("key"),
                        property.child_by_field_name("value"),
                    ),
                    _ => (None, None),
                };
                let local =
                    local.filter(|l| matches!(l.kind(), "identifier" | "shorthand_property_identifier_pattern"));
                if let (Some(imported), Some(local)) = (imported, local) {
                    let imported = specifier_name(imported, ctx.src);
                    ctx.import(&module, &imported, text(local, ctx.src), true);
                }
            }
        }
        _ => {}
    }
}
