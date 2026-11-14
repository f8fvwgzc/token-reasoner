//! Making a bulky tool output small without losing what the task needs.
//!
//! Three shorteners, each pure and deterministic:
//!
//! * a file read becomes a skeleton: definitions the task is not about keep
//!   their signature and lose their body, with a marker that says exactly how
//!   to read the lines that were left out;
//! * a search result is capped, with the files the task is about first;
//! * a check run (tests, lint, types) is reduced to its failures.
//!
//! A shortener returns `None` when it is not sure of the format or when the
//! saving is too small to be worth a changed output.

use lge_core::{kind, NodeLabel, NodeRec};
use lge_retrieve::budget::count;
use std::collections::BTreeMap;

/// An output smaller than this is never touched.
pub const MIN_TOKENS: usize = 400;
/// A shortened output must be at most this share of the original.
pub const MAX_RATIO: f64 = 0.7;

/// True when `after` is enough smaller than `before` to be worth sending instead.
fn worth_it(before: &str, after: &str) -> bool {
    let (b, a) = (count(before), count(after));
    b >= MIN_TOKENS && (a as f64) <= b as f64 * MAX_RATIO
}

// ---------------------------------------------------------------------------
// File skeleton
// ---------------------------------------------------------------------------

/// A body shorter than this is cheaper to send than to mark as left out.
const MIN_BODY_LINES: usize = 8;

fn comment_token(path: &str) -> &'static str {
    if path.ends_with(".py") {
        "#"
    } else {
        "//"
    }
}

/// Index (into `lines`) of the last line of the signature of a definition
/// that starts at `start`: the first line that opens the body.
fn signature_end(lines: &[&str], start: usize, end: usize) -> usize {
    for (i, line) in lines.iter().enumerate().take(end.min(start + 8) + 1).skip(start) {
        let t = line.trim_end();
        if t.trim_start().starts_with('@') {
            continue;
        }
        if t.ends_with(':') || t.ends_with('{') || t.ends_with("=> {") {
            return i;
        }
    }
    start
}

/// Shorten the text of a file read. `text` holds the file's lines from
/// `first_line` (1-based) on; `symbols` are the file's symbols from the graph;
/// `keep` says which symbols the task is about and must stay whole.
///
/// Functions and methods that are not kept and are long enough keep their
/// signature (and the first docstring line); the body is replaced by one
/// marker line that names the lines and the command that reads them.
pub fn skeleton(
    path: &str,
    text: &str,
    first_line: usize,
    symbols: &[NodeRec],
    keep: &dyn Fn(&NodeRec) -> bool,
) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let last_line = first_line + lines.len().saturating_sub(1);
    let hash = comment_token(path);
    let mut candidates: Vec<&NodeRec> = symbols
        .iter()
        .filter(|s| s.label == NodeLabel::Symbol && (s.kind == kind::FUNCTION || s.kind == kind::METHOD))
        .filter(|s| {
            s.start_line as usize >= first_line && s.end_line as usize <= last_line && s.end_line > s.start_line
        })
        .filter(|s| !keep(s))
        .collect();
    candidates.sort_by_key(|s| (s.start_line, std::cmp::Reverse(s.end_line)));

    // line index -> (last elided index, marker)
    let mut cuts: BTreeMap<usize, (usize, String)> = BTreeMap::new();
    let mut covered_to = 0usize; // 1-based line up to which an outer candidate already decided
    for s in candidates {
        let (start, end) = (s.start_line as usize, s.end_line as usize);
        if start <= covered_to {
            continue; // nested in a definition that was already handled
        }
        let (i0, i1) = (start - first_line, end - first_line);
        let mut from = signature_end(&lines, i0, i1) + 1;
        // Keep the first line of a docstring: it says what the function is for.
        if lines.get(from).is_some_and(|l| {
            l.trim_start().starts_with("\"\"\"")
                || l.trim_start().starts_with("'''")
                || l.trim_start().starts_with("/**")
        }) {
            from += 1;
        }
        // Keep a closing brace on its own line, so the structure still reads.
        let mut to = i1;
        if lines
            .get(to)
            .is_some_and(|l| matches!(l.trim(), "}" | "};" | "})" | "});"))
        {
            to = to.saturating_sub(1);
        }
        if from > to || to - from + 1 < MIN_BODY_LINES {
            continue;
        }
        covered_to = end;
        let indent: String = lines[from].chars().take_while(|c| c.is_whitespace()).collect();
        let (a, b) = (from + first_line, to + first_line);
        let marker = format!(
            "{indent}{hash} lge: {} lines of {} `{}` left out; read them with: sed -n '{a},{b}p' {path}",
            b - a + 1,
            s.kind,
            s.name
        );
        cuts.insert(from, (to, marker));
    }
    if cuts.is_empty() {
        return None;
    }
    let mut out = format!(
        "{hash} lge: shortened view of {path} (lines {first_line}-{last_line}): {} definition(s) this task does not involve are reduced to their signature.\n",
        cuts.len()
    );
    let mut i = 0;
    while i < lines.len() {
        match cuts.get(&i) {
            Some((to, marker)) => {
                out.push_str(marker);
                out.push('\n');
                i = to + 1;
            }
            None => {
                out.push_str(lines[i]);
                out.push('\n');
                i += 1;
            }
        }
    }
    worth_it(text, &out).then_some(out)
}

// ---------------------------------------------------------------------------
// Search results
// ---------------------------------------------------------------------------

/// Lines kept from one search.
const MAX_SEARCH_LINES: usize = 60;
/// Lines kept per file.
const MAX_PER_FILE: usize = 6;

/// The file a `path:line:text` or `path:text` match line belongs to.
fn match_path(line: &str) -> Option<&str> {
    let (path, rest) = line.split_once(':')?;
    if path.is_empty() || path.contains(' ') || path.len() > 300 || rest.is_empty() {
        return None;
    }
    // A path has a dot or a slash; `error: something` does not.
    (path.contains('/') || path.contains('.')).then_some(path)
}

/// Cap a long search result. Matches in files for which `relevant` is true
/// come first; every file keeps its first matches; the closing line says how
/// much was left out and how to see it.
pub fn cap_search(text: &str, relevant: &dyn Fn(&str) -> bool) -> Option<String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= MAX_SEARCH_LINES {
        return None;
    }
    let mut order: Vec<&str> = Vec::new();
    let mut by_file: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut parsed = 0usize;
    for line in &lines {
        let Some(path) = match_path(line) else { continue };
        parsed += 1;
        if !by_file.contains_key(path) {
            order.push(path);
        }
        by_file.entry(path).or_default().push(line);
    }
    // Not a list of matches (context lines, headings, another format): leave it.
    if parsed * 10 < lines.len() * 9 {
        return None;
    }
    // Stable: relevant files keep their order among themselves, then the rest.
    let (mut first, rest): (Vec<&str>, Vec<&str>) = order.iter().partition(|p| relevant(p));
    first.extend(rest);

    let mut out = String::new();
    let (mut shown, mut files_shown) = (0usize, 0usize);
    let mut skipped_files: Vec<&str> = Vec::new();
    for path in &first {
        let matches = &by_file[path];
        if shown >= MAX_SEARCH_LINES {
            skipped_files.push(path);
            continue;
        }
        let take = matches.len().min(MAX_PER_FILE).min(MAX_SEARCH_LINES - shown);
        for line in &matches[..take] {
            out.push_str(line);
            out.push('\n');
        }
        shown += take;
        files_shown += 1;
        if matches.len() > take {
            out.push_str(&format!(
                "{path}: (+{} more matching lines in this file)\n",
                matches.len() - take
            ));
        }
    }
    out.push_str(&format!(
        "lge: {shown} of {parsed} matching lines shown, from {files_shown} of {} files.",
        first.len()
    ));
    if !skipped_files.is_empty() {
        let names: Vec<&str> = skipped_files.iter().take(8).copied().collect();
        out.push_str(&format!(
            " Not shown: {}{}.",
            names.join(", "),
            if skipped_files.len() > names.len() { ", ..." } else { "" }
        ));
    }
    out.push_str(" Narrow the pattern or search one file to see the rest.\n");
    worth_it(text, &out).then_some(out)
}

// ---------------------------------------------------------------------------
// Check runs
// ---------------------------------------------------------------------------

/// Reduce the output of a test, lint or type-check run to its failures, or to
/// one line when it passed. `None` when the tool's output was not recognised:
/// an unknown format is never summarised.
pub fn failures_only(tool: &str, text: &str) -> Option<String> {
    let (failures, filtered) = lge_filter::filter_output(tool, text);
    let recognised = !failures.is_empty() || filtered.trim_start().starts_with("ok");
    if !recognised {
        return None;
    }
    let out = format!("{}\nlge: output of `{tool}` reduced to its result; run the command again with a narrower target for the full log.\n", filtered.trim_end());
    worth_it(text, &out).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, kind_: &str, start: i64, end: i64) -> NodeRec {
        let mut n = NodeRec::bare(name, NodeLabel::Symbol, name, name, kind_);
        n.start_line = start;
        n.end_line = end;
        n.file_path = "svc/orders.py".into();
        n
    }

    fn body(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("    value_{i} = compute_something(value_{i}, other_argument={i})\n"))
            .collect()
    }

    fn python_file() -> (String, Vec<NodeRec>) {
        let mut text = String::from("import os\n\n");
        let mut symbols = Vec::new();
        for (name, lines) in [
            ("create_order", 30usize),
            ("cancel_order", 30),
            ("tiny", 2),
            ("refund_order", 40),
        ] {
            let start = text.lines().count() as i64 + 1;
            text.push_str(&format!(
                "def {name}(order_id):\n    \"\"\"Docs of {name}.\"\"\"\n{}    return order_id\n\n",
                body(lines)
            ));
            symbols.push(sym(name, kind::FUNCTION, start, start + lines as i64 + 2));
        }
        (text, symbols)
    }

    #[test]
    fn a_file_read_keeps_what_the_task_is_about_and_marks_the_rest() {
        let (text, symbols) = python_file();
        let out = skeleton("svc/orders.py", &text, 1, &symbols, &|s| s.name == "cancel_order").unwrap();
        assert!(
            out.starts_with("# lge: shortened view of svc/orders.py (lines 1-120): 2 definition(s)"),
            "{out}"
        );
        // The kept function is whole.
        assert_eq!(out.matches("value_29 = compute_something").count(), 1, "{out}");
        assert!(
            out.contains("def cancel_order(order_id):\n    \"\"\"Docs of cancel_order.\"\"\"\n    value_0"),
            "{out}"
        );
        // The others keep signature and docstring, and say how to read the body.
        assert!(
            out.contains("def create_order(order_id):\n    \"\"\"Docs of create_order.\"\"\"\n    # lge: 31 lines of function `create_order` left out; read them with: sed -n '5,35p' svc/orders.py\n"),
            "{out}"
        );
        assert!(out.contains("left out; read them with: sed -n '"), "{out}");
        // A short function is not worth a marker.
        assert!(
            out.contains("def tiny(order_id):\n    \"\"\"Docs of tiny.\"\"\"\n    value_0"),
            "{out}"
        );
        assert!(count(&out) * 10 < count(&text) * 7);

        // The marker's range is exactly the lines that were left out.
        let original: Vec<&str> = text.lines().collect();
        assert!(original[4].contains("value_0") && original[34].trim() == "return order_id");
    }

    #[test]
    fn nothing_is_shortened_when_the_task_needs_it_all_or_the_file_is_small() {
        let (text, symbols) = python_file();
        assert_eq!(skeleton("svc/orders.py", &text, 1, &symbols, &|_| true), None);
        assert_eq!(
            skeleton(
                "svc/orders.py",
                "def a():\n    return 1\n",
                1,
                &[sym("a", kind::FUNCTION, 1, 2)],
                &|_| false
            ),
            None
        );
        // A read that starts further down: symbols outside it are ignored.
        let tail: String = text.lines().skip(40).map(|l| format!("{l}\n")).collect();
        let out = skeleton("svc/orders.py", &tail, 41, &symbols, &|_| false).unwrap();
        assert!(
            !out.contains("create_order") && out.contains("lines of function `refund_order` left out"),
            "{out}"
        );
    }

    #[test]
    fn braces_stay_and_nested_definitions_go_with_their_parent() {
        let mut text = String::from("export class Mailer {\n  async send(to: string) {\n");
        text.push_str(
            &"    await this.transport.sendMail({ to, subject: 'x', text: 'a long line of text here' });\n".repeat(40),
        );
        text.push_str("  }\n}\n");
        let symbols = [
            sym("Mailer", kind::CLASS, 1, 44),
            sym("send", kind::METHOD, 2, 43),
            sym("inner", kind::FUNCTION, 10, 30),
        ];
        let out = skeleton("src/mailer.ts", &text, 1, &symbols, &|_| false).unwrap();
        assert!(out.contains("  async send(to: string) {\n    // lge: 40 lines of method `send` left out; read them with: sed -n '3,42p' src/mailer.ts\n  }\n}\n"), "{out}");
        assert_eq!(
            out.matches("left out").count(),
            1,
            "the nested function is inside the method that was shortened"
        );
    }

    #[test]
    fn long_searches_are_capped_with_the_task_files_first() {
        let mut text = String::new();
        for f in 0..30 {
            for l in 0..10 {
                text.push_str(&format!(
                    "src/module_{f}/handlers.py:{}:    result = create_order(payload, request_id=request.id)\n",
                    l * 7 + 3
                ));
            }
        }
        let out = cap_search(&text, &|p| p == "src/module_17/handlers.py").unwrap();
        assert!(out.starts_with("src/module_17/handlers.py:3:"), "{out}");
        assert!(
            out.contains("src/module_17/handlers.py: (+4 more matching lines in this file)\n"),
            "{out}"
        );
        assert!(
            out.contains("lge: 60 of 300 matching lines shown, from 10 of 30 files. Not shown: "),
            "{out}"
        );
        assert!(out
            .trim_end()
            .ends_with("Narrow the pattern or search one file to see the rest."));
        // Short results, and output that is not a list of matches, are left alone.
        assert_eq!(cap_search("a.py:1:x\nb.py:2:y\n", &|_| false), None);
        let prose: String = (0..200)
            .map(|i| format!("line {i} of a build log without paths\n"))
            .collect();
        assert_eq!(cap_search(&prose, &|_| false), None);
    }

    #[test]
    fn a_passing_test_run_becomes_one_line_and_unknown_output_is_left_alone() {
        let mut raw = String::from(
            "============================= test session starts ==============================\ncollected 400 items\n\n",
        );
        for i in 0..400 {
            raw.push_str(&format!(
                "tests/test_orders.py::test_case_number_{i} PASSED                              [{:3}%]\n",
                i / 4
            ));
        }
        raw.push_str("\n============================= 400 passed in 12.34s =============================\n");
        let out = failures_only("pytest", &raw).unwrap();
        assert!(out.starts_with("ok") && out.contains("400 passed"), "{out}");
        assert!(count(&out) < 60, "{out}");
        let unknown: String = (0..300)
            .map(|i| format!("step {i}: doing some work that is not a known tool format\n"))
            .collect();
        assert_eq!(failures_only("pytest", &unknown), None);
    }
}
