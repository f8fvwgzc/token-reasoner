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
use std::collections::{BTreeMap, HashMap};

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
    lge_index::comment_token(path).unwrap_or("//")
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

/// The file a `path:line:text` or `path:text` match line belongs to. With
/// `numbered_only`, only a `path:line:text` line is a match.
fn match_path(line: &str, numbered_only: bool) -> Option<&str> {
    let (path, rest) = line.split_once(':')?;
    if path.is_empty() || path.contains(' ') || path.len() > 300 || rest.is_empty() {
        return None;
    }
    // `app.name: x` (YAML), `tests/a.py::test_b PASSED`, `host.lan:8080 up`.
    if numbered_only && !matches!(numbered(rest), Some((_, true))) {
        return None;
    }
    // A path has a dot or a slash; `error: something` does not.
    (path.contains('/') || path.contains('.')).then_some(path)
}

/// A note the agent itself puts into a long output (`Warning: truncated output
/// (original token count: N)`, `Total output lines: M`, `…N tokens truncated…`,
/// `…N chars truncated…`). It says what is missing, so it is never a match and
/// always stays.
fn is_note(line: &str) -> bool {
    line.contains("tokens truncated")
        || line.contains("chars truncated")
        || line.contains("truncated output")
        || line.starts_with("Total output lines")
}

/// `src/a.py` alone on a line: a file heading of rg's `--heading` layout.
fn heading_path(line: &str) -> Option<&str> {
    let ok = !line.is_empty()
        && line.len() <= 300
        && !line.contains(char::is_whitespace)
        && !line.contains(':')
        && !line.starts_with('-')
        && (line.contains('/') || line.contains('.'));
    ok.then_some(line)
}

/// `123:text` (a match, `true`) or `123-text` (context, `false`) under a heading.
fn numbered(line: &str) -> Option<(usize, bool)> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let number = line.get(..digits)?.parse().ok()?;
    match line.as_bytes().get(digits) {
        Some(b':') => Some((number, true)),
        Some(b'-') => Some((number, false)),
        _ => None,
    }
}

/// What one line of a search output is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Anything that is not a match: always kept, in place.
    Keep,
    /// A `path:line:text` match of file `.0`.
    Flat(usize),
    /// Heading layout: the path line of a file's group, its numbered lines
    /// (`matched` for `123:`), a `--` between them, and the blank line that
    /// closes the group.
    Head(usize),
    Line {
        file: usize,
        matched: bool,
    },
    Sep(usize),
    Tail(usize),
}

impl Role {
    fn file(self) -> Option<usize> {
        match self {
            Role::Keep => None,
            Role::Flat(f) | Role::Head(f) | Role::Sep(f) | Role::Tail(f) | Role::Line { file: f, .. } => Some(f),
        }
    }

    fn is_match(self) -> bool {
        matches!(self, Role::Flat(_) | Role::Line { matched: true, .. })
    }
}

/// The role of every line: groups of the heading layout first, then flat
/// match lines among the rest (only `path:line:text` ones with `numbered_only`).
/// Returns the roles and the file paths.
fn roles<'a>(lines: &[&'a str], numbered_only: bool) -> (Vec<Role>, Vec<&'a str>) {
    let mut role = vec![Role::Keep; lines.len()];
    let mut files: Vec<&'a str> = Vec::new();
    let mut index: HashMap<&'a str, usize> = HashMap::new();
    let mut file_id = |path: &'a str| -> usize {
        *index.entry(path).or_insert_with(|| {
            files.push(path);
            files.len() - 1
        })
    };
    let mut i = 0;
    while i < lines.len() {
        let head = heading_path(lines[i]).filter(|_| !is_note(lines[i]));
        let starts_group = lines.get(i + 1).is_some_and(|l| !is_note(l) && numbered(l).is_some());
        let (Some(path), true) = (head, starts_group) else {
            i += 1;
            continue;
        };
        let f = file_id(path);
        let mut group = vec![Role::Head(f)];
        let mut last_number = 0usize;
        let mut j = i + 1;
        while let Some(line) = lines.get(j) {
            if is_note(line) {
                break;
            }
            if *line == "--" {
                group.push(Role::Sep(f));
            } else {
                // Line numbers grow within a file; anything else ends the group.
                match numbered(line) {
                    Some((n, matched)) if n > last_number => {
                        last_number = n;
                        group.push(Role::Line { file: f, matched });
                    }
                    _ => break,
                }
            }
            j += 1;
        }
        // A `--` at the end separates groups; it is not part of this one.
        while group.last() == Some(&Role::Sep(f)) {
            group.pop();
            j -= 1;
        }
        if lines.get(j).is_some_and(|l| l.is_empty()) {
            group.push(Role::Tail(f));
            j += 1;
        }
        // A group without a match is not a search result.
        if group.iter().any(|r| r.is_match()) {
            role[i..j].copy_from_slice(&group);
        }
        i = j.max(i + 1);
    }
    for (line, r) in lines.iter().zip(role.iter_mut()) {
        if *r == Role::Keep && !is_note(line) {
            if let Some(path) = match_path(line, numbered_only) {
                *r = Role::Flat(file_id(path));
            }
        }
    }
    (role, files)
}

/// Cap a long search result. Lines that are not matches (notes, headers,
/// separators, `Binary file .. matches`) stay where they are; only the
/// matches are capped: files the task is about first, then the files with
/// the most matches, at most a few lines per file and about sixty in all.
/// Each capped file says how many of its lines were left out, and the closing
/// line says what was left out and how to see it.
///
/// Matches may come as `path:line:text` or `path:text` lines or in rg's
/// heading layout (a path line followed by `123:text` and `123-text` lines).
/// For the output of a command known to be a search. `None` when the output
/// is not mostly matches or capping saves too little.
pub fn cap_search(text: &str, relevant: &dyn Fn(&str) -> bool) -> Option<String> {
    cap(text, relevant, false)
}

/// [`cap_search`] for an output whose command is not known: only numbered
/// matches (`path:line:text`, or the heading layout) count, so YAML, logs and
/// test listings whose lines merely start with `name:` are never taken for a
/// search.
pub fn cap_numbered_search(text: &str, relevant: &dyn Fn(&str) -> bool) -> Option<String> {
    cap(text, relevant, true)
}

fn cap(text: &str, relevant: &dyn Fn(&str) -> bool, numbered_only: bool) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (role, files) = roles(&lines, numbered_only);
    let mut matches = vec![0usize; files.len()];
    for r in role.iter().filter(|r| r.is_match()) {
        if let Some(f) = r.file() {
            matches[f] += 1;
        }
    }
    let total: usize = matches.iter().sum();
    let non_blank = lines.iter().filter(|l| !l.trim().is_empty()).count();
    if total < MAX_SEARCH_LINES || total * 2 < non_blank {
        return None;
    }

    // Which files get room, best first; ties go by path so the order is fixed.
    let mut order: Vec<usize> = (0..files.len()).filter(|f| matches[*f] > 0).collect();
    order.sort_by_key(|f| (!relevant(files[*f]), std::cmp::Reverse(matches[*f]), files[*f]));
    let mut rank = vec![usize::MAX; files.len()];
    let mut take = vec![0usize; files.len()];
    let mut shown = 0usize;
    for (pos, f) in order.iter().enumerate() {
        rank[*f] = pos;
        take[*f] = matches[*f].min(MAX_PER_FILE).min(MAX_SEARCH_LINES - shown);
        shown += take[*f];
    }

    // Which lines stay. A file keeps its first matches; in the heading
    // layout, the lines up to its last kept match and that match's context.
    let mut keep = vec![false; lines.len()];
    let mut seen = vec![0usize; files.len()];
    for i in 0..lines.len() {
        keep[i] = match role[i] {
            Role::Keep => true,
            Role::Flat(f) | Role::Line { file: f, matched: true } => {
                seen[f] += 1;
                seen[f] <= take[f]
            }
            Role::Line {
                file: f,
                matched: false,
            } => {
                seen[f] < take[f]
                    || (seen[f] == take[f]
                        && take[f] > 0
                        && keep[i - 1]
                        && matches!(role[i - 1], Role::Line { file, .. } if file == f))
            }
            Role::Sep(f) => seen[f] < take[f],
            // Decided below, once the group's lines are.
            Role::Head(_) | Role::Tail(_) => false,
        };
    }
    // A heading and its closing blank line stay when a line of the group does.
    for i in 0..lines.len() {
        if !matches!(role[i], Role::Head(_)) {
            continue;
        }
        let end = (i + 1..lines.len())
            .find(|j| !matches!(role[*j], Role::Line { .. } | Role::Sep(_)))
            .unwrap_or(lines.len());
        let any = (i + 1..end).any(|j| keep[j]);
        keep[i] = any;
        if let Some(Role::Tail(_)) = role.get(end) {
            keep[end] = any;
        }
    }
    // The line after which a file's "+N more" note goes: its last kept line.
    let mut note_after = vec![usize::MAX; files.len()];
    for i in 0..lines.len() {
        if let (true, Role::Flat(f) | Role::Line { file: f, .. } | Role::Sep(f)) = (keep[i], role[i]) {
            note_after[f] = i;
        }
    }
    let note = |f: usize| {
        format!(
            "{}: (+{} more matching lines in this file)",
            files[f],
            matches[f] - take[f]
        )
    };

    let mut out: Vec<String> = Vec::new();
    // Something was left out since the last line written: a separator that
    // would now follow another separator is not repeated.
    let mut dropped = false;
    let mut i = 0;
    while i < lines.len() {
        if let Role::Flat(_) = role[i] {
            // A run of match lines: regrouped by file, best files first.
            let end = (i..lines.len())
                .find(|j| !matches!(role[*j], Role::Flat(_)))
                .unwrap_or(lines.len());
            // rank -> (file, its kept lines in this run)
            let mut by_rank: BTreeMap<usize, (usize, Vec<usize>)> = BTreeMap::new();
            for j in i..end {
                if let Role::Flat(f) = role[j] {
                    let entry = by_rank.entry(rank[f]).or_insert_with(|| (f, Vec::new()));
                    if keep[j] {
                        entry.1.push(j);
                    }
                }
            }
            let before = out.len();
            for (f, kept) in by_rank.into_values() {
                out.extend(kept.iter().map(|j| lines[*j].to_string()));
                if (i..end).contains(&note_after[f]) && matches[f] > take[f] {
                    out.push(note(f));
                }
            }
            // A run that wrote nothing was left out whole.
            dropped = out.len() == before;
            i = end;
            continue;
        }
        if !keep[i] {
            dropped = true;
            i += 1;
            continue;
        }
        let separator = |l: &str| l.trim().is_empty() || l == "--";
        if !(separator(lines[i]) && dropped && out.last().is_none_or(|l| separator(l))) {
            out.push(lines[i].to_string());
            dropped = false;
        }
        if let Some(f) = role[i].file() {
            if note_after[f] == i && matches[f] > take[f] {
                out.push(note(f));
            }
        }
        i += 1;
    }

    let files_shown = order.iter().filter(|f| take[**f] > 0).count();
    let skipped: Vec<&str> = order.iter().filter(|f| take[**f] == 0).map(|f| files[*f]).collect();
    let mut closing = format!(
        "lge: {shown} of {total} matching lines shown, from {files_shown} of {} files.",
        order.len()
    );
    if !skipped.is_empty() {
        let names: Vec<&str> = skipped.iter().take(8).copied().collect();
        closing.push_str(&format!(
            " Not shown: {}{}.",
            names.join(", "),
            if skipped.len() > names.len() { ", ..." } else { "" }
        ));
    }
    closing.push_str(" Narrow the pattern or search one file to see the rest.");
    out.push(closing);
    let out = out.join("\n") + "\n";
    worth_it(text, &out).then_some(out)
}

// ---------------------------------------------------------------------------
// Check runs
// ---------------------------------------------------------------------------

/// Reduce the output of a test, lint or type-check run to its failures, or to
/// one line when it passed. `None` when the tool's output was not recognised:
/// an unknown format is never summarised.
pub fn failures_only(tool: &str, text: &str) -> Option<String> {
    check_run(tool, text).flatten()
}

/// What [`failures_only`] decides, and whether the output was a check run at
/// all: `None` when it was not recognised, `Some(None)` when it was but is
/// sent as it is (the saving is too small, or it says nothing about a result).
///
/// A run is only called passed when the tool said so in its closing line (a
/// tally such as `300 passed`, or cargo's `Finished`): an output cut before
/// its end (a process still running, `| head`, a script that printed part of
/// it) has no failure in it yet, and that is not a pass.
pub fn check_run(tool: &str, text: &str) -> Option<Option<String>> {
    let (failures, filtered) = lge_filter::filter_output(tool, text);
    let passed = failures.is_empty() && filtered.trim_start().starts_with("ok");
    if failures.is_empty() && !passed {
        return None;
    }
    let finished = filtered.trim_start().starts_with("ok:")
        || text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .is_some_and(|l| l.trim_start().starts_with("Finished "));
    if passed && !finished {
        return Some(None);
    }
    let out = format!("{}\nlge: output of `{tool}` reduced to its result; run the command again with a narrower target for the full log.\n", filtered.trim_end());
    Some(worth_it(text, &out).then_some(out))
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

    fn flat(file: &str, line: usize) -> String {
        format!("{file}:{line}:    result = create_order(payload, request_id=request.id)\n")
    }

    fn module(f: usize) -> String {
        format!("src/module_{f}/handlers.py")
    }

    #[test]
    fn notes_headers_and_binary_lines_stay_in_place_around_capped_matches() {
        let mut text = String::from(
            "Warning: truncated output (original token count: 48211)\nTotal output lines: 2210\n\n== matches for create_order ==\n",
        );
        for f in 0..20 {
            for l in 0..10 {
                text.push_str(&flat(&module(f), l * 7 + 3));
            }
        }
        // The agent cut the middle of the output, inside a line that looks like a match.
        text.push_str("src/module_3/handlers.py:99:    x = create_ord…31877 tokens truncated…\n");
        for f in 20..30 {
            for l in 0..10 {
                text.push_str(&flat(&module(f), l * 7 + 3));
            }
        }
        text.push_str("Binary file assets/bundle.min.js matches\n");
        let out = cap_search(&text, &|p| p == "src/module_25/handlers.py").unwrap();
        assert!(
            out.starts_with("Warning: truncated output (original token count: 48211)\nTotal output lines: 2210\n\n== matches for create_order ==\nsrc/module_0/handlers.py:3:"),
            "{out}"
        );
        let cut = out
            .find("src/module_3/handlers.py:99:    x = create_ord…31877 tokens truncated…\n")
            .expect("the agent's note stays");
        let task_file = out
            .find("src/module_25/handlers.py:3:")
            .expect("the task's file is shown");
        let binary = out.find("Binary file assets/bundle.min.js matches\n").expect("kept");
        assert!(
            cut < task_file && task_file < binary,
            "every line stays in its place: {out}"
        );
        assert!(out.contains("src/module_25/handlers.py: (+4 more matching lines in this file)\n"));
        assert!(
            out.ends_with("lge: 60 of 300 matching lines shown, from 10 of 30 files. Not shown: src/module_17/handlers.py, src/module_18/handlers.py, src/module_19/handlers.py, src/module_2/handlers.py, src/module_20/handlers.py, src/module_21/handlers.py, src/module_22/handlers.py, src/module_23/handlers.py, .... Narrow the pattern or search one file to see the rest.\n"),
            "{out}"
        );
        assert!(count(&out) * 10 <= count(&text) * 7);
    }

    #[test]
    fn separators_stay_and_do_not_pile_up_where_matches_were_left_out() {
        for separator in ["--", ""] {
            let mut text = String::new();
            for f in 0..30 {
                for l in 0..10 {
                    text.push_str(&flat(&module(f), l + 1));
                }
                text.push_str(separator);
                text.push('\n');
            }
            let out = cap_search(&text, &|_| false).unwrap();
            let twice = format!("\n{separator}\n{separator}\n");
            assert!(!out.contains(&twice), "{out}");
            assert_eq!(
                out.lines().filter(|l| *l == separator).count(),
                10,
                "one after each file shown: {out}"
            );
            assert!(out
                .starts_with("src/module_0/handlers.py:1:    result = create_order(payload, request_id=request.id)\n"));
        }
    }

    #[test]
    fn the_heading_layout_is_capped_per_file() {
        let mut text = String::new();
        for f in 0..30 {
            text.push_str(&format!("{}\n", module(f)));
            for l in 0..10 {
                text.push_str(&format!("{}:    result = create_order(payload)\n", l * 7 + 3));
            }
            text.push('\n');
        }
        let out = cap_search(&text, &|p| p == "src/module_17/handlers.py").unwrap();
        assert!(
            out.contains("src/module_17/handlers.py\n3:    result = create_order(payload)\n"),
            "{out}"
        );
        assert!(
            !out.contains("src/module_2/handlers.py\n"),
            "a file that is not shown loses its heading too: {out}"
        );
        assert!(!out.contains("\n\n\n"), "{out}");
        assert!(
            out.starts_with("src/module_0/handlers.py\n3:"),
            "groups keep their order: {out}"
        );
        assert!(out.contains(
            "38:    result = create_order(payload)\nsrc/module_17/handlers.py: (+4 more matching lines in this file)\n\n"
        ));
        assert!(
            out.contains("lge: 60 of 300 matching lines shown, from 10 of 30 files."),
            "{out}"
        );
    }

    #[test]
    fn context_lines_of_the_heading_layout_go_with_their_match() {
        let group = "1-// orders\n2:create_order(a)\n3:create_order(b)\n4:create_order(c)\n5-}\n--\n20-// refunds\n21:create_order(d)\n22:create_order(e)\n23:create_order(f)\n24-}\n--\n40-// exports\n41:create_order(g)\n42:create_order(h)\n43:create_order(i)\n44:create_order(j)\n45-}\n";
        let mut text = String::new();
        for f in 0..20 {
            text.push_str(&format!("src/m{f:02}.py\n{group}\n"));
        }
        let out = cap_search(&text, &|_| false).unwrap();
        assert!(
            out.starts_with("src/m00.py\n1-// orders\n2:create_order(a)\n3:create_order(b)\n4:create_order(c)\n5-}\n--\n20-// refunds\n21:create_order(d)\n22:create_order(e)\n23:create_order(f)\n24-}\nsrc/m00.py: (+4 more matching lines in this file)\n\nsrc/m01.py\n"),
            "{out}"
        );
        assert!(!out.contains("src/m10.py\n"), "{out}");
        assert!(out.contains("lge: 60 of 200 matching lines shown, from 10 of 20 files."));
    }

    #[test]
    fn output_that_is_not_mostly_matches_is_left_alone() {
        // Too few matches.
        let few: String = (0..59).map(|i| flat(&module(i), 1)).collect();
        assert_eq!(cap_search(&few, &|_| false), None);
        // Many matches, but more lines that are not.
        let mut mixed: String = (0..100).map(|i| flat(&module(i % 10), i)).collect();
        for i in 0..120 {
            mixed.push_str(&format!(
                "step {i}: compiling a crate that has nothing to do with the search\n"
            ));
        }
        assert_eq!(cap_search(&mixed, &|_| false), None);
        // A log under a file name is not the heading layout: its numbers do not grow.
        let mut log = String::from("app.log\n");
        for i in 0..200 {
            log.push_str(&format!(
                "12:30:{:02} worker {i} picked up job {i} from the queue\n",
                i % 60
            ));
        }
        assert_eq!(cap_search(&log, &|_| false), None);
    }

    #[test]
    fn capping_that_saves_too_little_is_not_worth_a_changed_output() {
        // Seventy files with one match each: sixty are shown, which is most of it.
        let text: String = (0..70).map(|i| flat(&module(i), 4)).collect();
        assert_eq!(cap_search(&text, &|_| false), None);
    }

    #[test]
    fn files_are_ordered_by_task_then_matches_then_path() {
        let mut text = String::new();
        let mut add = |file: &str, n: usize| {
            for l in 0..n {
                text.push_str(&flat(file, l + 1));
            }
        };
        add("lib/b.py", 20);
        add("lib/z.py", 3);
        add("lib/a.py", 20);
        add("lib/c.py", 30);
        for i in 0..50 {
            add(&format!("lib/one_{i:02}.py"), 1);
        }
        let out = cap_search(&text, &|p| p == "lib/z.py").unwrap();
        let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s} in {out}"));
        assert!(at("lib/z.py:1:") < at("lib/c.py:1:"));
        assert!(at("lib/c.py:1:") < at("lib/a.py:1:"));
        assert!(at("lib/a.py:1:") < at("lib/b.py:1:"));
        assert!(at("lib/b.py:1:") < at("lib/one_00.py:1:"));
        assert!(at("lib/one_00.py:1:") < at("lib/one_38.py:1:"));
        assert!(!out.contains("lib/one_39.py:1:"), "{out}");
        assert_eq!(cap_search(&text, &|p| p == "lib/z.py").as_deref(), Some(out.as_str()));
    }

    #[test]
    fn a_cut_inside_a_match_line_stays_whichever_unit_it_counts() {
        for marker in ["…31877 tokens truncated…", "…120455 chars truncated…"] {
            let mut text = String::new();
            for f in 0..30 {
                for l in 0..10 {
                    text.push_str(&flat(&module(f), l + 1));
                }
            }
            text.push_str(&format!(
                "src/module_29/handlers.py:99:    x = create_ord{marker}rest(payload)\n"
            ));
            let out = cap_search(&text, &|_| false).unwrap();
            assert!(out.contains(marker), "the agent's note stays: {out}");
        }
    }

    #[test]
    fn without_a_known_command_only_numbered_matches_are_taken_for_a_search() {
        // A config file with dotted keys, a test listing, a host list: lines
        // that start with `name:` but are not matches.
        let yaml: String = (0..120)
            .map(|i| format!("app.feature_{i}.enabled: true # toggles feature number {i} for the service\n"))
            .collect();
        let listing: String = (0..300)
            .map(|i| {
                format!(
                    "tests/test_orders_{}.py::test_case_number_{i} PASSED      [{:3}%]\n",
                    i % 10,
                    i / 3
                )
            })
            .collect();
        let hosts: String = (0..150)
            .map(|i| format!("node-{i}.cluster.local:8080 healthy, last answered a probe in 12 ms\n"))
            .collect();
        for text in [&yaml, &listing, &hosts] {
            assert_eq!(cap_numbered_search(text, &|_| false), None, "{text}");
        }
        // The same text from a known search command is a search: `rg` without `-n`.
        assert!(cap_search(&yaml, &|_| false).is_some());
        // Numbered matches and the heading layout are still capped.
        let numbered: String = (0..30)
            .flat_map(|f| (0..10).map(move |l| flat(&module(f), l + 1)))
            .collect();
        assert_eq!(
            cap_numbered_search(&numbered, &|_| false),
            cap_search(&numbered, &|_| false)
        );
        assert!(cap_numbered_search(&numbered, &|_| false).is_some());
        let mut heading = String::new();
        for f in 0..30 {
            heading.push_str(&format!("{}\n", module(f)));
            for l in 0..10 {
                heading.push_str(&format!("{}:    result = create_order(payload)\n", l * 7 + 3));
            }
            heading.push('\n');
        }
        assert!(cap_numbered_search(&heading, &|_| false).is_some());
    }

    #[test]
    fn a_run_cut_before_its_result_is_not_called_passed() {
        let mut partial = String::from(
            "============================= test session starts ==============================\ncollected 300 items\n\n",
        );
        for i in 0..200 {
            partial.push_str(&format!(
                "tests/test_orders.py::test_case_number_{i} PASSED                              [{:3}%]\n",
                i / 3
            ));
        }
        // Still running, or cut by `| head`: no failure yet is not a pass.
        assert_eq!(check_run("pytest", &partial), Some(None));
        assert_eq!(failures_only("pytest", &partial), None);
        let mut build: String = (0..80)
            .map(|i| format!("   Compiling crate_number_{i} v0.{i}.0 (/repo/crates/crate_number_{i})\n"))
            .collect();
        assert_eq!(check_run("cargo", &build), Some(None));
        // Finished: the build passed.
        build.push_str("    Finished `dev` profile [unoptimized + debuginfo] target(s) in 41.20s\n");
        let out = failures_only("cargo", &build).unwrap();
        assert!(out.starts_with("ok\n"), "{out}");
        // A failure in a cut run is still reported.
        partial.push_str("tests/test_orders.py::test_case_number_200 FAILED                              [ 66%]\n");
        assert!(failures_only("pytest", &partial).is_some_and(|o| o.contains("test_case_number_200")));
        // Not a check run at all.
        assert_eq!(check_run("", "just some words\n"), None);
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
