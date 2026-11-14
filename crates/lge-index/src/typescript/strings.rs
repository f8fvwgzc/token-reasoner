//! String-valued expressions taken apart into literal text and interpolations,
//! and names written with dots.

use super::text;
use crate::facts::{push_text, Piece};
use tree_sitter::Node;

/// A string-valued expression, taken apart.
pub(super) struct Text {
    pub(super) pieces: Vec<Piece>,
    /// A string literal, or a template string without `${}`: what
    /// `FactSite::literal` asks for.
    pub(super) plain: bool,
    /// Ids of the nodes that were read, so they are not searched for SQL again.
    pub(super) nodes: Vec<usize>,
}

/// Reads `node` as a string: a literal, a template string, or a `+` chain with
/// at least one string in it. Parentheses, `as const` and `!` around it are looked through.
pub(super) fn text_of(node: Node, src: &[u8]) -> Option<Text> {
    let mut t = Text {
        pieces: Vec::new(),
        plain: true,
        nodes: Vec::new(),
    };
    add_text(node, src, &mut t, 0).then_some(t)
}

fn is_string(node: Node) -> bool {
    matches!(node.kind(), "string" | "template_string")
}

/// Appends `node` to `t`. Returns false, with `t` untouched, when it is not a string.
fn add_text(node: Node, src: &[u8], t: &mut Text, depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    match node.kind() {
        "string" | "template_string" => string_pieces(node, src, t),
        "parenthesized_expression" | "as_expression" | "satisfies_expression" | "non_null_expression" => {
            // The value comes first; `as` and `satisfies` are followed by a type.
            let mut c = node.walk();
            let inner = node.named_children(&mut c).find(|n| n.kind() != "comment");
            let Some(inner) = inner else { return false };
            if !add_text(inner, src, t, depth + 1) {
                return false;
            }
        }
        "binary_expression" => {
            let (operands, chain) = concat_operands(node);
            if operands.len() < 2 || !operands.iter().any(|o| is_string(*o)) {
                return false;
            }
            for o in operands {
                if !add_text(o, src, t, depth + 1) {
                    t.pieces.push(Piece::Hole(expr_text(o, src)));
                }
            }
            t.nodes.extend(chain);
            t.plain = false;
        }
        _ => return false,
    }
    t.nodes.push(node.id());
    true
}

fn is_plus(node: Node) -> bool {
    node.kind() == "binary_expression" && node.child_by_field_name("operator").is_some_and(|o| o.kind() == "+")
}

/// Operands of a left-nested `a + b + c` chain, in source order, and the ids of
/// the `+` nodes inside it. Iterative, because such chains can be very long.
fn concat_operands(node: Node) -> (Vec<Node>, Vec<usize>) {
    let mut operands = Vec::new();
    let mut chain = Vec::new();
    let mut cur = node;
    while let (Some(left), Some(right), true) = (
        cur.child_by_field_name("left"),
        cur.child_by_field_name("right"),
        is_plus(cur),
    ) {
        chain.push(cur.id());
        operands.push(right);
        cur = left;
    }
    operands.push(cur);
    operands.reverse();
    (operands, chain)
}

fn string_pieces(string: Node, src: &[u8], t: &mut Text) {
    let mut c = string.walk();
    for part in string.named_children(&mut c) {
        match part.kind() {
            "string_fragment" => push_text(&mut t.pieces, text(part, src)),
            "escape_sequence" => push_text(&mut t.pieces, &unescape(text(part, src))),
            "template_substitution" => {
                let mut c2 = part.walk();
                let expr = part.named_children(&mut c2).find(|n| n.kind() != "comment");
                t.pieces
                    .push(Piece::Hole(expr.map(|e| expr_text(e, src)).unwrap_or_default()));
                t.plain = false;
            }
            _ => {}
        }
    }
    t.nodes.push(string.id());
}

/// The character an escape sequence stands for, so that `"SELECT *\nFROM t"`
/// reads as two words around the line break. Unknown escapes are kept as written.
fn unescape(raw: &str) -> String {
    match raw {
        "\\n" => "\n".into(),
        "\\t" => "\t".into(),
        "\\r" => "\r".into(),
        "\\\\" => "\\".into(),
        "\\'" => "'".into(),
        "\\\"" => "\"".into(),
        "\\`" => "`".into(),
        "\\$" => "$".into(),
        "\\/" => "/".into(),
        // A backslash at the end of a line continues the string.
        "\\\n" | "\\\r\n" => String::new(),
        _ => raw.into(),
    }
}

/// The text of a string literal without interpolation (`"./x"`, `` `./x` ``).
pub(super) fn plain_string(node: Node, src: &[u8]) -> Option<String> {
    if !is_string(node) {
        return None;
    }
    let t = text_of(node, src).filter(|t| t.plain)?;
    crate::facts::plain_text(&t.pieces)
}

/// Source text of an expression on one line, cut at 80 chars.
pub(super) fn expr_text(node: Node, src: &[u8]) -> String {
    text(node, src)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

/// `a`, `a.b`, `this.a.b`, `A.B` in a type: a name written with dots, or `None`
/// for anything else. `a?.b` and `a!.b` read as `a.b`.
pub(super) fn dotted(node: Node, src: &[u8]) -> Option<String> {
    let mut parts = Vec::new();
    let mut cur = node;
    loop {
        match cur.kind() {
            "identifier" | "type_identifier" | "this" | "super" => {
                parts.push(text(cur, src));
                break;
            }
            "member_expression" => {
                let property = cur.child_by_field_name("property")?;
                if !matches!(property.kind(), "property_identifier" | "private_property_identifier") {
                    return None;
                }
                parts.push(text(property, src));
                cur = cur.child_by_field_name("object")?;
            }
            "nested_type_identifier" => {
                parts.push(text(cur.child_by_field_name("name")?, src));
                cur = cur.child_by_field_name("module")?;
            }
            "nested_identifier" => {
                parts.push(text(cur.child_by_field_name("property")?, src));
                cur = cur.child_by_field_name("object")?;
            }
            "non_null_expression" => cur = cur.named_child(0)?,
            _ => return None,
        }
    }
    parts.reverse();
    Some(parts.join("."))
}
