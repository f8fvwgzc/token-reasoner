//! String-valued expressions taken apart into literal text and interpolations.

use super::text;
use crate::facts::{push_text, Piece};
use tree_sitter::Node;

/// A string-valued expression, taken apart.
pub(super) struct Text {
    pub(super) pieces: Vec<Piece>,
    /// A string literal (or adjacent literals) with no interpolation, formatting
    /// or `+`: what `FactSite::literal` asks for.
    pub(super) plain: bool,
    /// Ids of the nodes that were read, so they are not searched for SQL again.
    pub(super) nodes: Vec<usize>,
}

/// Reads `node` as a string: a literal, an f-string, adjacent literals, a `+`
/// chain with at least one string in it, `"..." % args` or `"...".format(args)`.
pub(super) fn text_of(node: Node, src: &[u8]) -> Option<Text> {
    let mut t = Text {
        pieces: Vec::new(),
        plain: true,
        nodes: Vec::new(),
    };
    add_text(node, src, &mut t, 0).then_some(t)
}

/// Appends `node` to `t`. Returns false, with `t` untouched, when it is not a string.
fn add_text(node: Node, src: &[u8], t: &mut Text, depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    match node.kind() {
        "string" => string_pieces(node, src, t),
        "concatenated_string" => {
            let mut c = node.walk();
            for part in node.named_children(&mut c).filter(|p| p.kind() == "string") {
                string_pieces(part, src, t);
            }
        }
        "parenthesized_expression" => {
            let mut c = node.walk();
            let inner: Vec<Node> = node.named_children(&mut c).filter(|n| n.kind() != "comment").collect();
            if inner.len() != 1 || !add_text(inner[0], src, t, depth + 1) {
                return false;
            }
        }
        "binary_operator" => {
            let op = node.child_by_field_name("operator").map(|o| o.kind()).unwrap_or("");
            match op {
                "+" => {
                    let (operands, chain) = concat_operands(node);
                    if !operands
                        .iter()
                        .any(|o| matches!(o.kind(), "string" | "concatenated_string"))
                    {
                        return false;
                    }
                    for o in operands {
                        if !add_text(o, src, t, depth + 1) {
                            t.pieces.push(Piece::Hole(expr_text(o, src)));
                        }
                    }
                    t.nodes.extend(chain);
                }
                // `"... %s" % value`: the text is the left side.
                "%" => {
                    let Some(left) = node.child_by_field_name("left") else {
                        return false;
                    };
                    if !add_text(left, src, t, depth + 1) {
                        return false;
                    }
                }
                _ => return false,
            }
            t.plain = false;
        }
        "call" => {
            // `"...{}".format(value)`: the text is the receiver.
            let Some(f) = node.child_by_field_name("function").filter(|f| f.kind() == "attribute") else {
                return false;
            };
            let is_format = f
                .child_by_field_name("attribute")
                .is_some_and(|a| text(a, src) == "format");
            let Some(object) = f.child_by_field_name("object").filter(|_| is_format) else {
                return false;
            };
            if !add_text(object, src, t, depth + 1) {
                return false;
            }
            t.plain = false;
        }
        _ => return false,
    }
    t.nodes.push(node.id());
    true
}

/// Operands of a left-nested `a + b + c` chain, in source order, and the ids of
/// the `+` nodes inside it. Iterative, because such chains can be very long.
fn concat_operands(node: Node) -> (Vec<Node>, Vec<usize>) {
    let mut operands = Vec::new();
    let mut chain = Vec::new();
    let mut cur = node;
    loop {
        let is_plus =
            cur.kind() == "binary_operator" && cur.child_by_field_name("operator").is_some_and(|o| o.kind() == "+");
        let (Some(left), Some(right), true) = (
            cur.child_by_field_name("left"),
            cur.child_by_field_name("right"),
            is_plus,
        ) else {
            break;
        };
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
    for part in string.children(&mut c) {
        match part.kind() {
            "string_content" => push_text(&mut t.pieces, &content_text(part, src)),
            "interpolation" => {
                let expr = part
                    .child_by_field_name("expression")
                    .map(|e| expr_text(e, src))
                    .unwrap_or_default();
                t.pieces.push(Piece::Hole(expr));
                t.plain = false;
            }
            _ => {}
        }
    }
    t.nodes.push(string.id());
}

/// Text of a `string_content` node with the common escapes decoded, so that
/// `"SELECT *\nFROM t"` reads as two words around the line break.
fn content_text(content: Node, src: &[u8]) -> String {
    let mut out = String::new();
    let mut pos = content.start_byte();
    let mut c = content.walk();
    for esc in content.children(&mut c) {
        out.push_str(&String::from_utf8_lossy(&src[pos..esc.start_byte()]));
        let raw = text(esc, src);
        match raw {
            "\\n" => out.push('\n'),
            "\\t" => out.push('\t'),
            "\\r" => out.push('\r'),
            "\\\\" => out.push('\\'),
            "\\'" => out.push('\''),
            "\\\"" => out.push('"'),
            "{{" => out.push('{'),
            "}}" => out.push('}'),
            // A backslash at the end of a line continues the string.
            "\\\n" | "\\\r\n" => {}
            _ => out.push_str(raw),
        }
        pos = esc.end_byte();
    }
    out.push_str(&String::from_utf8_lossy(&src[pos..content.end_byte()]));
    out
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

/// `a`, `a.b`, `a.b.c`: a name written with dots, or `None` for anything else.
pub(super) fn dotted(node: Node, src: &[u8]) -> Option<String> {
    let mut parts = Vec::new();
    let mut cur = node;
    loop {
        match cur.kind() {
            "identifier" => {
                parts.push(text(cur, src));
                break;
            }
            "attribute" => {
                parts.push(text(cur.child_by_field_name("attribute")?, src));
                cur = cur.child_by_field_name("object")?;
            }
            _ => return None,
        }
    }
    parts.reverse();
    Some(parts.join("."))
}
