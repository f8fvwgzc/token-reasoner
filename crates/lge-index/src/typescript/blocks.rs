//! Test blocks: `describe("title", ...)` groups tests, and the callback of
//! `it("title", ...)` or `test("title", ...)` is one test.

use super::strings::{expr_text, text_of};
use super::{arguments, is_function_value, text};
use crate::facts::template_key;
use tree_sitter::Node;

/// Longest symbol name made from a test title, in chars.
const TITLE_CHARS: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TestKind {
    /// `describe("title", () => { ... })`: a group of tests.
    Suite,
    /// `it("title", () => { ... })`, `test("title", () => { ... })`: one test.
    Case,
}

pub(super) struct TestBlock<'t> {
    pub(super) kind: TestKind,
    pub(super) title: String,
    pub(super) callback: Node<'t>,
}

/// Modifiers between the test function and its call: `it.only`, `test.each(table)`.
const TEST_MODIFIERS: &[&str] = &[
    "only",
    "skip",
    "each",
    "concurrent",
    "sequential",
    "serial",
    "parallel",
    "failing",
    "fails",
    "todo",
    "runIf",
    "skipIf",
];

/// What the callee of a call says about it: `describe`, `it.only`,
/// `test.each(table)`, Playwright's `test.describe`.
pub(super) fn test_callee(f: Node, src: &[u8]) -> Option<TestKind> {
    let mut cur = f;
    let mut suite = false;
    for _ in 0..8 {
        match cur.kind() {
            "identifier" => {
                let kind = match text(cur, src) {
                    "describe" | "fdescribe" | "xdescribe" | "context" | "suite" => TestKind::Suite,
                    "it" | "test" | "fit" | "xit" | "xtest" | "specify" => TestKind::Case,
                    _ => return None,
                };
                return Some(if suite { TestKind::Suite } else { kind });
            }
            "member_expression" => {
                let modifier = text(cur.child_by_field_name("property")?, src);
                if modifier == "describe" {
                    suite = true;
                } else if !TEST_MODIFIERS.contains(&modifier) {
                    return None;
                }
                cur = cur.child_by_field_name("object")?;
            }
            "call_expression" => cur = cur.child_by_field_name("function")?,
            _ => return None,
        }
    }
    None
}

/// `describe("title", callback)` or `it("title", callback)`, when `call` is one.
pub(super) fn test_block<'t>(call: Node<'t>, src: &[u8]) -> Option<TestBlock<'t>> {
    let kind = test_callee(call.child_by_field_name("function")?, src)?;
    let args = arguments(call);
    let title = *args.first()?;
    let callback = args.iter().skip(1).copied().find(|a| is_function_value(*a))?;
    let title = match text_of(title, src) {
        Some(t) => template_key(&t.pieces),
        None => expr_text(title, src),
    };
    Some(TestBlock { kind, title, callback })
}

/// A test title as a symbol name: letters, digits and `_`, everything else
/// folded into single underscores.
pub(super) fn sanitise(title: &str) -> String {
    let mut out = String::new();
    let mut len = 0;
    for ch in title.chars() {
        if len >= TITLE_CHARS {
            break;
        }
        if ch.is_alphanumeric() || ch == '_' {
            out.push(ch);
            len += 1;
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
            len += 1;
        }
    }
    let name = out.trim_matches('_');
    if name.is_empty() {
        "test".to_string()
    } else {
        name.to_string()
    }
}
