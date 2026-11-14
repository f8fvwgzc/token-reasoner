//! Doc comments: the comment above a definition, and the one that opens a file.

use super::{children_of_kind, is_class_value, is_function_value, text, unwrap};
use tree_sitter::Node;

/// Longest doc line kept on a symbol, in chars.
const DOC_CHARS: usize = 200;

/// The first line of a comment that says something: comment markers, JSDoc tags
/// and tool directives are passed over.
fn comment_line(raw: &str) -> String {
    let body = match raw.strip_prefix("/*") {
        Some(block) => block.strip_suffix("*/").unwrap_or(block),
        None => raw.trim_start_matches('/'),
    };
    let directive = |line: &str| {
        [
            "@",
            "eslint",
            "prettier-",
            "istanbul ",
            "c8 ",
            "tslint:",
            "biome-",
            "<reference",
            "<amd",
        ]
        .iter()
        .any(|d| line.starts_with(d))
    };
    body.lines()
        .map(|line| line.trim().trim_start_matches('*').trim())
        .find(|line| !line.is_empty() && !directive(line))
        .map(|line| line.chars().take(DOC_CHARS).collect())
        .unwrap_or_default()
}

fn is_block_comment(comment: Node, src: &[u8]) -> bool {
    text(comment, src).starts_with("/*")
}

/// Doc of a definition that starts at `first`: the JSDoc block or the run of
/// `//` lines directly above it.
pub(super) fn leading_doc(first: Node, src: &[u8]) -> String {
    let mut block: Vec<Node> = Vec::new();
    let mut below = first;
    let mut cur = first.prev_named_sibling();
    while let Some(comment) = cur {
        // A blank line detaches a comment from what follows.
        if comment.kind() != "comment" || comment.end_position().row + 1 < below.start_position().row {
            break;
        }
        // A comment at the end of a line of code belongs to that code.
        let trailing = comment
            .prev_sibling()
            .is_some_and(|p| p.kind() != "comment" && p.end_position().row == comment.start_position().row);
        if trailing {
            break;
        }
        block.push(comment);
        if is_block_comment(comment, src) {
            break;
        }
        below = comment;
        cur = comment.prev_named_sibling();
    }
    block
        .iter()
        .rev()
        .map(|c| comment_line(text(*c, src)))
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// Doc of the module: the comment that opens the file, unless it sits directly
/// on top of the first definition and documents that instead.
pub(super) fn module_doc(root: Node, src: &[u8]) -> String {
    let mut block: Vec<Node> = Vec::new();
    let mut next = None;
    let mut c = root.walk();
    for child in root.named_children(&mut c) {
        if child.kind() == "hash_bang_line" && block.is_empty() {
            continue;
        }
        let joins = child.kind() == "comment"
            && block.last().is_none_or(|prev: &Node| {
                !is_block_comment(*prev, src)
                    && !is_block_comment(child, src)
                    && prev.end_position().row + 1 == child.start_position().row
            });
        if !joins {
            next = Some(child);
            break;
        }
        block.push(child);
    }
    let Some(last) = block.last() else {
        return String::new();
    };
    let documents_next =
        next.is_some_and(|n| n.start_position().row <= last.end_position().row + 1 && defines_symbol(n));
    if documents_next {
        return String::new();
    }
    block
        .iter()
        .map(|c| comment_line(text(*c, src)))
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// True for a statement whose leading comment is the doc of a symbol:
/// a declaration, or a variable bound to a function or a class.
fn defines_symbol(statement: Node) -> bool {
    match statement.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "abstract_class_declaration"
        | "interface_declaration"
        | "type_alias_declaration"
        | "enum_declaration" => true,
        "export_statement" => {
            let exported = statement
                .child_by_field_name("declaration")
                .or_else(|| statement.child_by_field_name("value"));
            exported.is_some_and(|e| defines_symbol(e) || is_function_value(unwrap(e)) || is_class_value(unwrap(e)))
        }
        "lexical_declaration" | "variable_declaration" => children_of_kind(statement, "variable_declarator")
            .into_iter()
            .filter_map(|d| d.child_by_field_name("value"))
            .any(|v| is_function_value(unwrap(v)) || is_class_value(unwrap(v))),
        _ => false,
    }
}
