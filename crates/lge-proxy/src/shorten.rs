//! Making a bulky tool output small without losing what the task needs.
//!
//! The shorteners here, each pure and deterministic:
//!
//! * a file read becomes a skeleton: definitions the task is not about keep
//!   their signature and lose their body, with a marker that says exactly how
//!   to read the lines that were left out;
//! * a search result is capped, with the files the task is about first, and
//!   its very long match lines can be clipped around the match;
//! * a check run (tests, lint, types) is reduced to its result;
//! * terminal noise and repeated lines are cleaned ([`inert`]), and JSON loses
//!   the whitespace between its tokens ([`minify_json`]).
//!
//! Path lists, lock files, progress noise and re-run deltas have their own
//! modules: [`crate::fold`], [`crate::lockfile`], [`crate::noise`] and
//! [`crate::delta`].
//!
//! A shortener returns `None` (or "as is") when it is not sure of the format
//! or when the saving is too small to be worth a changed output.

use crate::command::SearchArgs;
use lge_core::{kind, NodeLabel, NodeRec};
use lge_retrieve::budget::count;
use regex::{Regex, RegexBuilder};
use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

/// An output smaller than this is never touched.
pub const MIN_TOKENS: usize = 400;
/// A shortened output must be at most this share of the original.
pub const MAX_RATIO: f64 = 0.7;

/// True when `after` is enough smaller than `before` to be worth sending instead.
pub(crate) fn worth_it(before: &str, after: &str) -> bool {
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

/// The number prefix of a line of `nl -ba` or `cat -n`: `<spaces><digits>\t`.
/// The line number and the prefix's length in bytes.
fn number_prefix(line: &str) -> Option<(usize, usize)> {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    let digits = line[spaces..].bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || line.as_bytes().get(spaces + digits) != Some(&b'\t') {
        return None;
    }
    let number = line[spaces..spaces + digits].parse().ok()?;
    Some((number, spaces + digits + 1))
}

/// Shorten the text of a file read. `text` holds the file's lines from
/// `first_line` (1-based) on; `symbols` are the file's symbols from the graph;
/// `keep` says which symbols the task is about and must stay whole.
///
/// Functions and methods that are not kept and are long enough keep their
/// signature (and the first docstring line); the body is replaced by one
/// marker line that names the lines and the command that reads them.
///
/// With `numbered`, `text` is the output of `nl -ba` or `cat -n`: every line
/// must start with its number and a tab, the numbers rising by one from
/// `first_line`, or nothing is shortened. The prefixes are set aside to read
/// the code; lines that are kept keep them, and markers name real line numbers.
pub fn skeleton(
    path: &str,
    text: &str,
    first_line: usize,
    symbols: &[NodeRec],
    keep: &dyn Fn(&NodeRec) -> bool,
    numbered: bool,
) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    // The code of each line, without its number prefix.
    let mut code: Vec<&str> = Vec::with_capacity(lines.len());
    let mut prefix_len: Vec<usize> = Vec::with_capacity(lines.len());
    for (k, line) in lines.iter().enumerate() {
        let len = if numbered {
            let (number, len) = number_prefix(line)?;
            if number != first_line + k {
                return None;
            }
            len
        } else {
            0
        };
        code.push(&line[len..]);
        prefix_len.push(len);
    }
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
        let mut from = signature_end(&code, i0, i1) + 1;
        // Keep the first line of a docstring: it says what the function is for.
        if code.get(from).is_some_and(|l| {
            l.trim_start().starts_with("\"\"\"")
                || l.trim_start().starts_with("'''")
                || l.trim_start().starts_with("/**")
        }) {
            from += 1;
        }
        // Keep a closing brace on its own line, so the structure still reads.
        let mut to = i1;
        if code
            .get(to)
            .is_some_and(|l| matches!(l.trim(), "}" | "};" | "})" | "});"))
        {
            to = to.saturating_sub(1);
        }
        if from > to || to - from + 1 < MIN_BODY_LINES {
            continue;
        }
        covered_to = end;
        let indent: String = code[from].chars().take_while(|c| c.is_whitespace()).collect();
        // A marker of a numbered read lines up with the code: blank number, tab.
        let pad = if numbered {
            format!("{}\t", " ".repeat(prefix_len[from].saturating_sub(1)))
        } else {
            String::new()
        };
        let (a, b) = (from + first_line, to + first_line);
        let marker = format!(
            "{pad}{indent}{hash} lge: {} lines of {} `{}` left out; read them with: sed -n '{a},{b}p' {path}",
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
/// Tokens the list of files left out of a capped search may take.
const NOT_SHOWN_BUDGET: usize = 1000;
/// Lines kept per file.
const MAX_PER_FILE: usize = 6;

/// The file a `path:line:text` or `path:text` match line belongs to. With
/// `numbered_only`, only a `path:line:text` line is a match. A path with a
/// space is only taken from a numbered line of a known search (not with
/// `numbered_only`): in other output, `Total lines: 12:` is not a path.
fn match_path(line: &str, numbered_only: bool) -> Option<&str> {
    let (path, rest) = line.split_once(':')?;
    if path.trim().len() != path.len() || path.is_empty() || path.len() > 300 || rest.is_empty() {
        return None;
    }
    let is_numbered = matches!(numbered(rest), Some((_, true)));
    if path.contains(' ') && (numbered_only || !is_numbered) {
        return None;
    }
    // `app.name: x` (YAML), `tests/a.py::test_b PASSED`, `host.lan:8080 up`.
    if numbered_only && !is_numbered {
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

/// The files a capped search left out, best first, each with its number of
/// matching lines: `a.py (12), b.py (3)`. Every one is named while the list
/// fits in [`NOT_SHOWN_BUDGET`] tokens; past that, how many more there are and
/// how many lines they have.
fn not_shown(skipped: &[usize], files: &[&str], matches: &[usize]) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    let mut named = 0usize;
    for f in skipped {
        let entry = format!("{} ({})", files[*f], matches[*f]);
        // +1 for the ", " that joins it.
        let cost = count(&entry) + 1;
        if used + cost > NOT_SHOWN_BUDGET {
            break;
        }
        if named > 0 {
            out.push_str(", ");
        }
        out.push_str(&entry);
        used += cost;
        named += 1;
    }
    let rest = &skipped[named..];
    if !rest.is_empty() {
        let lines: usize = rest.iter().map(|f| matches[*f]).sum();
        out.push_str(&format!(
            "{}{} more files ({lines} matching lines; rg -l with the same pattern lists them)",
            if named > 0 { ", and " } else { "" },
            rest.len()
        ));
    }
    out
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
    let skipped: Vec<usize> = order.iter().copied().filter(|f| take[*f] == 0).collect();
    let mut closing = format!(
        "lge: {shown} of {total} matching lines shown, from {files_shown} of {} files.",
        order.len()
    );
    if !skipped.is_empty() {
        closing.push_str(" Not shown: ");
        closing.push_str(&not_shown(&skipped, &files, &matches));
        closing.push('.');
    }
    closing.push_str(
        " Narrow the pattern or search one file to see the rest, or run the same search again before changing anything to see it whole.",
    );
    out.push(closing);
    let out = out.join("\n") + "\n";
    worth_it(text, &out).then_some(out)
}

// ---------------------------------------------------------------------------
// Check runs
// ---------------------------------------------------------------------------

/// What the agent's frame says about the process that printed an output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunStatus {
    /// Its exit code, when it has exited and the code is known.
    pub exit: Option<i32>,
    /// It is still running: what was printed so far is not all of it.
    pub running: bool,
    /// The status the shell reported is not the run's verdict (a pipe without
    /// `pipefail`, `|| true`, several statements joined by `;`): the output may
    /// be a fragment or hold another program's failure, so the run is never
    /// called passed.
    pub masked: bool,
}

/// What [`check_run`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckDecision {
    /// The output is not a test, lint, type-check or build run lge recognises.
    NotACheck,
    /// It is one, but it is sent as it is: something follows the tool's
    /// result, a failure is not certain to be all there is, or the saving is
    /// too small.
    AsIs,
    /// It is one, and this is its result, to send in its place.
    Shorter(String),
}

/// Reduce the output of a test, lint, type-check or build run to its result:
/// every failure, and every warning grouped by rule with all its locations;
/// one line for a pass. `tool` picks the parser (`""` lets the output decide);
/// `label` names the command in the marker (`tool` when empty, then "the
/// command").
///
/// A run is only called passed when the tool's own closing line is in the
/// output and nothing but the tool's epilogue follows it, the process is not
/// still running, its exit code, if known, is 0, the shell did not mask the
/// status (a pipe without `pipefail`, `|| true`, statements joined by `;`),
/// no line says the run failed (cargo's `error: 1 target failed:`, go's
/// `FAIL`, `make: *** ... Error 1`, `BUILD FAILED`), and the tool did not
/// count failures that no parser read. A non-zero exit with no
/// failure parsed is sent as it is: lge does not know why it failed. Output
/// that continues after the tool's result (`cargo run`, `pytest && script`)
/// is always sent as it is.
pub fn check_run(tool: &str, label: &str, text: &str, status: RunStatus) -> CheckDecision {
    let opts = lge_filter::FilterOptions {
        passed: None,
        keep_warnings: true,
        ..lge_filter::FilterOptions::default()
    };
    let filtered = lge_filter::filter_for_proxy(tool, text, &opts);
    let failed = !filtered.failures.is_empty();
    if !filtered.recognised && !failed {
        return CheckDecision::NotACheck;
    }
    if filtered.trailing {
        return CheckDecision::AsIs;
    }
    // A pass needs exit 0, or an unknown status that is not masked (a code-mode
    // part prints a whole output without its status).
    let exit_ok = match status.exit {
        Some(code) => code == 0,
        None => !status.masked,
    };
    // The tool counted failures no parser read, or a line says the run failed:
    // not a pass lge can vouch for.
    if !failed && (!exit_ok || status.masked || !filtered.closed || status.running || filtered.unparsed > 0) {
        return CheckDecision::AsIs;
    }
    let label = match (label, tool) {
        ("", "") => "the command".to_string(),
        ("", tool) => format!("`{tool}`"),
        (label, _) => format!("`{label}`"),
    };
    let warnings = match filtered.warnings {
        0 => String::new(),
        1 => " and its 1 warning (every location listed)".to_string(),
        n => format!(" and its {n} warnings (every location listed)"),
    };
    let out = format!(
        "{}\nlge: output of {label} reduced to its result{warnings}. Running the same command again before changing anything prints it whole.\n",
        filtered.text.trim_end()
    );
    if worth_it(text, &out) {
        CheckDecision::Shorter(out)
    } else {
        CheckDecision::AsIs
    }
}

/// [`check_run`] for a finished run of `tool` whose exit code is not known:
/// the shorter text, or `None`. Kept for tests.
pub fn failures_only(tool: &str, text: &str) -> Option<String> {
    match check_run(tool, tool, text, RunStatus::default()) {
        CheckDecision::Shorter(out) => Some(out),
        CheckDecision::NotACheck | CheckDecision::AsIs => None,
    }
}

// ---------------------------------------------------------------------------
// Long match lines
// ---------------------------------------------------------------------------

/// A match whose text is longer than this many characters is clipped.
const CLIP_OVER: usize = 500;
/// Characters kept of a clipped match.
const CLIP_KEEP: usize = 300;
/// Characters kept before the first match of the pattern.
const CLIP_BEFORE: usize = 100;

/// The search's pattern as a regular expression, when it compiles.
fn search_regex(args: &SearchArgs) -> Option<Regex> {
    let pattern = args.pattern.as_deref().filter(|p| !p.is_empty())?;
    let mut source = if args.fixed {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    if args.word {
        source = format!(r"\b(?:{source})\b");
    }
    let insensitive = args.ignore_case || (args.smart_case && !pattern.chars().any(char::is_uppercase));
    RegexBuilder::new(&source)
        .case_insensitive(insensitive)
        .size_limit(1 << 20)
        .build()
        .ok()
}

/// A path as a shell word: single-quoted when it has a character outside
/// `[A-Za-z0-9._/-]`.
pub(crate) fn shell_word(path: &str) -> String {
    if path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
    {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

/// The clipped form of the match text `body` of line `number` of `path`, or
/// `None` when it is short enough.
fn clip_body(body: &str, re: Option<&Regex>, path: &str, number: usize) -> Option<String> {
    let total = body.chars().count();
    if total <= CLIP_OVER {
        return None;
    }
    let first = re
        .and_then(|re| re.find(body))
        .map_or(0, |m| body[..m.start()].chars().count());
    // A match near the end still gets a whole window.
    let start = first.saturating_sub(CLIP_BEFORE).min(total - CLIP_KEEP);
    let window: String = body.chars().skip(start).take(CLIP_KEEP).collect();
    Some(format!(
        "{}{window}...[lge: +{} chars; whole line: sed -n '{number}p' {}]",
        if start > 0 { "..." } else { "" },
        total - CLIP_KEEP,
        shell_word(path)
    ))
}

/// Clip the very long lines of a search result: a numbered match
/// (`path:line:text`, or `line:text` under a heading path) whose text is over
/// 500 characters keeps 300 characters from 100 before the pattern's first
/// match (from the start when there is no pattern, it does not compile or it
/// does not match), and says how many characters were left out and how to
/// print the whole line. Unnumbered matches and context lines are never
/// clipped. `None` when nothing was clipped; the caller decides whether the
/// result is worth sending.
pub fn clip_match_lines(text: &str, args: &SearchArgs) -> Option<String> {
    let segments: Vec<&str> = text.split_inclusive('\n').collect();
    let lines: Vec<&str> = segments
        .iter()
        .map(|s| s.strip_suffix('\n').map_or(*s, |l| l.strip_suffix('\r').unwrap_or(l)))
        .collect();
    let (role, files) = roles(&lines, false);
    let re = search_regex(args);
    let mut out = String::with_capacity(text.len());
    let mut clipped = false;
    for (i, segment) in segments.iter().enumerate() {
        let line = lines[i];
        let (prefix_len, path) = match role[i] {
            Role::Flat(f) => (files[f].len() + 1, files[f]),
            Role::Line { file, matched: true } => (0, files[file]),
            _ => (0, ""),
        };
        let rest = &line[prefix_len..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let numbered_match = !path.is_empty() && digits > 0 && rest.as_bytes().get(digits) == Some(&b':');
        let short = numbered_match
            .then(|| rest[..digits].parse::<usize>().ok())
            .flatten()
            .and_then(|number| clip_body(&rest[digits + 1..], re.as_ref(), path, number));
        match short {
            Some(body) => {
                out.push_str(&line[..prefix_len + digits + 1]);
                out.push_str(&body);
                out.push_str(&segment[line.len()..]);
                clipped = true;
            }
            None => out.push_str(segment),
        }
    }
    clipped.then_some(out)
}

// ---------------------------------------------------------------------------
// Lossless cleaning
// ---------------------------------------------------------------------------

/// The output as a terminal would show it ([`lge_filter::normalize_terminal`]:
/// no escape codes, carriage returns applied, no trailing blanks), with runs
/// of identical lines collapsed ([`lge_filter::collapse_repeats`]). `None`
/// when that changes nothing.
pub fn inert(text: &str) -> Option<String> {
    let clean = lge_filter::normalize_terminal(text);
    let collapsed = lge_filter::collapse_repeats(&clean);
    (collapsed.as_ref() != text).then(|| collapsed.into_owned())
}

/// Only the terminal part of [`inert`]: escape codes removed and carriage
/// returns applied. For output whose format is not known, where even trailing
/// blanks and repeats are left as they are. `None` when the text has no ESC
/// or carriage return, or when cleaning changes nothing.
pub fn terminal_only(text: &str) -> Option<String> {
    if !text.contains(['\u{1b}', '\r', '\u{9b}']) {
        return None;
    }
    let clean = lge_filter::strip_ansi(text);
    (clean != text).then_some(clean)
}

/// Largest JSON text [`minify_json`] reads.
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;

/// One JSON value without the whitespace between its tokens. Nothing is
/// re-encoded: numbers, escapes and key order stay byte for byte. The text
/// must be one whole JSON object or array (a second value, as in NDJSON,
/// refuses) of at most 8 MB, and the result must parse again and be worth
/// sending.
pub fn minify_json(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) || trimmed.len() > MAX_JSON_BYTES {
        return None;
    }
    serde_json::from_str::<serde::de::IgnoredAny>(trimmed).ok()?;
    let mut out: Vec<u8> = Vec::with_capacity(trimmed.len());
    let (mut in_string, mut escaped) = (false, false);
    for &b in trimmed.as_bytes() {
        if in_string {
            out.push(b);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => {}
            b'"' => {
                in_string = true;
                out.push(b);
            }
            _ => out.push(b),
        }
    }
    // Only ASCII bytes were removed, so the rest is still UTF-8.
    let out = String::from_utf8(out).ok()?;
    serde_json::from_str::<serde::de::IgnoredAny>(&out).ok()?;
    worth_it(text, &out).then_some(out)
}

/// True when RTK has condensed the output already (its recall hints, its
/// stamps, its notes about what it left out). Such output gets no lossy rule.
pub fn condensed_upstream(text: &str) -> bool {
    static MARKS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"rtk recall [0-9a-f]{6,}|\[rtk:|lines omitted by rtk|\[full output: |full output in recall file\]|\[full diff: rtk ",
        )
        .expect("the pattern is a valid regex")
    });
    MARKS.is_match(text)
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
        let out = skeleton(
            "svc/orders.py",
            &text,
            1,
            &symbols,
            &|s| s.name == "cancel_order",
            false,
        )
        .unwrap();
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
        assert_eq!(skeleton("svc/orders.py", &text, 1, &symbols, &|_| true, false), None);
        assert_eq!(
            skeleton(
                "svc/orders.py",
                "def a():\n    return 1\n",
                1,
                &[sym("a", kind::FUNCTION, 1, 2)],
                &|_| false,
                false
            ),
            None
        );
        // A read that starts further down: symbols outside it are ignored.
        let tail: String = text.lines().skip(40).map(|l| format!("{l}\n")).collect();
        let out = skeleton("svc/orders.py", &tail, 41, &symbols, &|_| false, false).unwrap();
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
        let out = skeleton("src/mailer.ts", &text, 1, &symbols, &|_| false, false).unwrap();
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
        assert!(out.ends_with(
            " Narrow the pattern or search one file to see the rest, or run the same search again before changing anything to see it whole.\n"
        ));
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
        let hidden: Vec<String> = [17, 18, 19, 2, 20, 21, 22, 23, 24, 26, 27, 28, 29, 3, 4, 5, 6, 7, 8, 9]
            .iter()
            .map(|f| format!("{} (10)", module(*f)))
            .collect();
        assert!(
            out.ends_with(&format!("lge: 60 of 300 matching lines shown, from 10 of 30 files. Not shown: {}. Narrow the pattern or search one file to see the rest, or run the same search again before changing anything to see it whole.\n", hidden.join(", "))),
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
        // From a known search command, `path:text` lines are matches: `rg` without `-n`.
        let unnumbered: String = (0..30)
            .flat_map(|f| (0..10).map(move |l| format!("{}:    result = create_order(payload, {l})\n", module(f))))
            .collect();
        assert!(cap_search(&unnumbered, &|_| false).is_some());
        assert_eq!(cap_numbered_search(&unnumbered, &|_| false), None);
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

    fn pytest_lines(n: usize) -> String {
        let mut run = String::from(
            "============================= test session starts ==============================\ncollected 300 items\n\n",
        );
        for i in 0..n {
            run.push_str(&format!(
                "tests/test_orders.py::test_case_number_{i} PASSED                              [{:3}%]\n",
                i / 3
            ));
        }
        run
    }

    fn exited(code: i32) -> RunStatus {
        RunStatus {
            exit: Some(code),
            running: false,
            masked: false,
        }
    }

    #[test]
    fn a_run_cut_before_its_tally_is_not_called_passed() {
        let mut partial = pytest_lines(200);
        // Still running, or cut by `| head`: no failure yet is not a pass.
        assert_eq!(
            check_run("pytest", "pytest", &partial, RunStatus::default()),
            CheckDecision::AsIs
        );
        assert_eq!(failures_only("pytest", &partial), None);
        let mut build: String = (0..80)
            .map(|i| format!("   Compiling crate_number_{i} v0.{i}.0 (/repo/crates/crate_number_{i})\n"))
            .collect();
        assert_eq!(check_run("cargo", "cargo", &build, exited(0)), CheckDecision::AsIs);
        // Finished: the build passed.
        build.push_str("    Finished `dev` profile [unoptimized + debuginfo] target(s) in 41.20s\n");
        let out = failures_only("cargo", &build).unwrap();
        assert!(out.starts_with("ok\n"), "{out}");
        assert!(out.ends_with("lge: output of `cargo` reduced to its result. Running the same command again before changing anything prints it whole.\n"), "{out}");
        // A failure in a cut run is still reported.
        partial.push_str("tests/test_orders.py::test_case_number_200 FAILED                              [ 66%]\n");
        assert!(failures_only("pytest", &partial).is_some_and(|o| o.contains("test_case_number_200")));
        // Not a check run at all.
        assert_eq!(
            check_run("", "", "just some words\n", RunStatus::default()),
            CheckDecision::NotACheck
        );
    }

    #[test]
    fn a_running_process_is_never_called_passed() {
        let mut run = pytest_lines(300);
        run.push_str("\n============================= 300 passed in 9.87s ==============================\n");
        let running = RunStatus {
            exit: None,
            running: true,
            masked: false,
        };
        assert_eq!(check_run("pytest", "pytest", &run, running), CheckDecision::AsIs);
        assert!(matches!(
            check_run("pytest", "pytest", &run, RunStatus::default()),
            CheckDecision::Shorter(_)
        ));
        assert!(matches!(
            check_run("pytest", "pytest", &run, exited(0)),
            CheckDecision::Shorter(_)
        ));
    }

    #[test]
    fn a_nonzero_exit_without_parsed_failures_is_as_is() {
        let mut run = pytest_lines(300);
        run.push_str("\n============================= 300 passed in 9.87s ==============================\n");
        // The tally says passed, the exit code says otherwise: lge does not know why.
        assert_eq!(check_run("pytest", "pytest", &run, exited(1)), CheckDecision::AsIs);
        assert_eq!(check_run("pytest", "pytest", &run, exited(-9)), CheckDecision::AsIs);
        // A tally that counts a failure no parser read is not a pass either.
        let mut unread = pytest_lines(300);
        unread.push_str("\n======================== 1 failed, 299 passed in 9.87s =========================\n");
        assert_eq!(
            check_run("pytest", "pytest", &unread, RunStatus::default()),
            CheckDecision::AsIs
        );
        // With a failure parsed, a non-zero exit is the expected case.
        let mut failed = pytest_lines(299);
        failed.push_str("tests/test_orders.py::test_case_number_299 FAILED                              [ 99%]\n\n=================================== FAILURES ===================================\n__________________________ test_case_number_299 ___________________________\n\n    def test_case_number_299():\n>       assert total() == 3\nE       assert 2 == 3\n\ntests/test_orders.py:12: AssertionError\n=========================== short test summary info ============================\nFAILED tests/test_orders.py::test_case_number_299 - assert 2 == 3\n======================== 1 failed, 299 passed in 9.87s =========================\n");
        match check_run("pytest", "pytest", &failed, exited(1)) {
            CheckDecision::Shorter(out) => {
                assert!(
                    out.contains("test_case_number_299") && out.contains("tests/test_orders.py"),
                    "{out}"
                )
            }
            other => panic!("{other:?}"),
        }
    }

    fn fixture(name: &str) -> String {
        let path = format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn cargo_run_with_a_warning_and_a_panic_is_sent_as_is() {
        // A real capture: one build warning, `Finished`, 120 rows of the
        // program's output, then its panic.
        let raw = fixture("cargo_run_panic.txt");
        assert!(raw.contains("panicked at src/main.rs:7:23"));
        for status in [RunStatus::default(), exited(101), exited(0)] {
            assert_eq!(
                check_run("cargo", "cargo", &raw, status),
                CheckDecision::AsIs,
                "{status:?}"
            );
        }
        // Judged by content alone, too.
        assert_eq!(check_run("", "", &raw, RunStatus::default()), CheckDecision::AsIs);
    }

    #[test]
    fn a_check_chained_with_another_program_is_sent_as_is() {
        let mut run = pytest_lines(300);
        run.push_str("\n============================= 300 passed in 9.87s ==============================\n");
        run.push_str("Traceback (most recent call last):\n  File \"scripts/report.py\", line 3, in <module>\n    main()\nKeyError: 'total'\n");
        assert_eq!(check_run("", "", &run, RunStatus::default()), CheckDecision::AsIs);
        assert_eq!(check_run("pytest", "pytest", &run, exited(1)), CheckDecision::AsIs);
    }

    /// `cargo clippy` with `n` warnings over three lints.
    fn clippy_warnings(n: usize) -> String {
        let lints = [
            ("needless_return", "unneeded `return` statement", "remove `return`"),
            ("redundant_clone", "redundant clone", "remove this"),
            ("len_zero", "length comparison to zero", "using `is_empty` is clearer"),
        ];
        let mut out = String::from("    Checking shop v0.1.0 (/repo)\n");
        for i in 0..n {
            let (lint, message, help) = lints[i % 3];
            out.push_str(&format!(
                "warning: {message}\n  --> src/m{i}.rs:{}:5\n   |\n{} |     return value_{i}.clone();\n   |     ^^^^^^^^^^^^^^^^^^^^^^^^\n   |\n   = help: for further information visit https://rust-lang.github.io/rust-clippy/master/index.html#{lint}\n   = note: `#[warn(clippy::{lint})]` on by default\nhelp: {help}\n   |\n{} -     return value_{i}.clone();\n{} +     value_{i}\n   |\n\n",
                10 + i,
                10 + i,
                10 + i,
                10 + i
            ));
        }
        out.push_str(&format!(
            "warning: `shop` (lib) generated {n} warnings (run `cargo clippy --fix --lib -p shop` to apply {n} suggestions)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.52s\n"
        ));
        out
    }

    #[test]
    fn clippy_warnings_keep_every_location() {
        let raw = clippy_warnings(30);
        let out = match check_run("cargo", "cargo", &raw, exited(0)) {
            CheckDecision::Shorter(out) => out,
            other => panic!("{other:?}"),
        };
        for i in 0..30 {
            assert!(out.contains(&format!("src/m{i}.rs:{}", 10 + i)), "src/m{i}.rs: {out}");
        }
        assert!(
            out.ends_with("lge: output of `cargo` reduced to its result and its 30 warnings (every location listed). Running the same command again before changing anything prints it whole.\n"),
            "{out}"
        );
        assert!(
            count(&out) * 10 <= count(&raw) * 3,
            "{} of {}",
            count(&out),
            count(&raw)
        );
    }

    #[test]
    fn a_passing_test_run_becomes_one_line_and_unknown_output_is_left_alone() {
        let mut raw = pytest_lines(400);
        raw.push_str("\n============================= 400 passed in 12.34s =============================\n");
        let out = failures_only("pytest", &raw).unwrap();
        assert!(out.starts_with("ok") && out.contains("400 passed"), "{out}");
        assert!(count(&out) < 60, "{out}");
        let unknown: String = (0..300)
            .map(|i| format!("step {i}: doing some work that is not a known tool format\n"))
            .collect();
        assert_eq!(failures_only("pytest", &unknown), None);
        // Without a tool, the label is the command.
        match check_run("", "", &raw, RunStatus::default()) {
            CheckDecision::Shorter(out) => assert!(
                out.contains("lge: output of the command reduced to its result."),
                "{out}"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_hidden_file_of_a_capped_search_is_named_with_its_count() {
        let mut text = String::new();
        for f in 0..30 {
            for l in 0..(30 - f) {
                text.push_str(&flat(&module(f), l + 1));
            }
        }
        let out = cap_search(&text, &|_| false).unwrap();
        let closing = out.lines().last().unwrap();
        // Ten files are shown; the other twenty are named, best first.
        let hidden: Vec<String> = (10..30).map(|f| format!("{} ({})", module(f), 30 - f)).collect();
        assert!(
            closing.contains(&format!("Not shown: {}.", hidden.join(", "))),
            "{closing}"
        );
        assert!(!closing.contains("more files"), "{closing}");
    }

    #[test]
    fn a_search_over_thousands_of_files_stays_within_the_list_budget() {
        let mut text = String::new();
        for f in 0..2000 {
            for l in 0..(1 + f % 3) {
                text.push_str(&flat(&format!("src/pkg_{f:04}/handlers.py"), l + 1));
            }
        }
        let total: usize = (0..2000).map(|f| 1 + f % 3).sum();
        let out = cap_search(&text, &|_| false).unwrap();
        let closing = out.lines().last().unwrap();
        let list = closing
            .split_once("Not shown: ")
            .and_then(|(_, rest)| rest.split_once(", and "))
            .map(|(list, _)| list)
            .expect("a long list ends with a count");
        assert!(count(list) <= NOT_SHOWN_BUDGET, "{}", count(list));
        let named = list.split(", ").count();
        // The count of what is not named is exact.
        let shown_files: usize = closing
            .split_once(", from ")
            .and_then(|(_, r)| r.split_once(' '))
            .and_then(|(n, _)| n.parse().ok())
            .unwrap();
        let more = 2000 - shown_files - named;
        let named_lines: usize = list
            .split(", ")
            .map(|e| {
                e.rsplit_once(" (")
                    .unwrap()
                    .1
                    .trim_end_matches(')')
                    .parse::<usize>()
                    .unwrap()
            })
            .sum();
        let more_lines = total - 60 - named_lines;
        assert!(
            closing.contains(&format!(
                ", and {more} more files ({more_lines} matching lines; rg -l with the same pattern lists them). Narrow"
            )),
            "{closing}"
        );
    }

    #[test]
    fn file_names_with_spaces_stay_whole() {
        let mut text = String::new();
        for f in 0..30 {
            for l in 0..10 {
                text.push_str(&flat(&format!("docs/my notes {f:02}/read me.md"), l + 1));
            }
        }
        let out = cap_search(&text, &|_| false).unwrap();
        assert!(out.contains("docs/my notes 00/read me.md:1:"), "{out}");
        assert!(
            out.contains("Not shown: docs/my notes 10/read me.md (10), docs/my notes 11/read me.md (10)"),
            "{out}"
        );
        // Without a known search command, such lines are not taken for matches.
        assert_eq!(cap_numbered_search(&text, &|_| false), None);
    }

    fn args(pattern: &str) -> SearchArgs {
        SearchArgs {
            pattern: Some(pattern.to_string()),
            ..SearchArgs::default()
        }
    }

    #[test]
    fn a_minified_line_is_clipped_around_its_match() {
        let filler = "var a=function(b){return b+1};".repeat(700);
        let line = format!("{filler}createOrder(payload){filler}");
        let total = line.chars().count();
        let text = format!("dist/app.min.js:1:{line}\nsrc/a.ts:3:const x = createOrder(y);\n");
        let out = clip_match_lines(&text, &args("createOrder")).unwrap();
        let first = out.lines().next().unwrap();
        assert!(first.starts_with("dist/app.min.js:1:..."), "{first}");
        assert!(first.contains("createOrder(payload)"), "{first}");
        assert!(
            first.ends_with(&format!(
                "...[lge: +{} chars; whole line: sed -n '1p' dist/app.min.js]",
                total - 300
            )),
            "{first}"
        );
        // 100 characters before the match.
        let window = first
            .strip_prefix("dist/app.min.js:1:...")
            .and_then(|w| w.split_once("...[lge:"))
            .unwrap()
            .0;
        assert_eq!(window.chars().count(), 300);
        assert_eq!(window.find("createOrder"), Some(100));
        assert!(
            out.ends_with("\nsrc/a.ts:3:const x = createOrder(y);\n"),
            "short lines stay"
        );
    }

    #[test]
    fn clipping_honours_the_search_flags() {
        let body = |needle: &str| format!("{}{needle}{}", "x".repeat(2000), "y".repeat(2000));
        // -F: the pattern's metacharacters are literal.
        let text = format!("a.js:7:{}\n", body("a.b(c"));
        let fixed = SearchArgs {
            fixed: true,
            ..args("a.b(c")
        };
        assert!(clip_match_lines(&text, &fixed).unwrap().contains("a.b(c"));
        // -i: the case of the match does not matter.
        let text = format!("a.js:7:{}\n", body("CreateOrder"));
        let insensitive = SearchArgs {
            ignore_case: true,
            ..args("createorder")
        };
        assert!(clip_match_lines(&text, &insensitive).unwrap().contains("CreateOrder"));
        // No match (or a pattern that does not compile): the start of the line.
        for unmatched in [args("not-there"), args("(unclosed")] {
            let out = clip_match_lines(&text, &unmatched).unwrap();
            assert!(
                out.starts_with(&format!("a.js:7:{}...[lge: +", "x".repeat(300))),
                "{out}"
            );
        }
        // Lines of 500 characters or less are left alone; so is output without numbers.
        let exactly = format!("a.js:7:{}\n", "z".repeat(500));
        assert_eq!(clip_match_lines(&exactly, &args("z")), None);
        let unnumbered = format!("a.js:{}\n", "z".repeat(5000));
        assert_eq!(clip_match_lines(&unnumbered, &args("z")), None);
    }

    #[test]
    fn clipping_cuts_on_character_boundaries() {
        let body = format!("{}needle{}", "é日".repeat(800), "ü本".repeat(800));
        let text = format!("src/ünï code.txt:12:{body}\n");
        let out = clip_match_lines(&text, &args("needle")).unwrap();
        assert!(out.starts_with("src/ünï code.txt:12:...é日"), "{out}");
        assert!(out.contains("needle"));
        assert!(
            out.ends_with(&format!(
                "...[lge: +{} chars; whole line: sed -n '12p' 'src/ünï code.txt']\n",
                body.chars().count() - 300
            )),
            "{out}"
        );
        // The heading layout keeps its own prefix and names the heading's path.
        let heading = format!("src/b.rs\n4:{body}\n");
        let out = clip_match_lines(&heading, &args("needle")).unwrap();
        assert!(out.starts_with("src/b.rs\n4:..."), "{out}");
        assert!(out.contains("whole line: sed -n '4p' src/b.rs]"), "{out}");
    }

    #[test]
    fn json_loses_only_its_whitespace() {
        let mut pretty = String::from("{\n  \"items\": [\n");
        for i in 0..200 {
            pretty.push_str(&format!(
                "    {{\n      \"id\": {i},\n      \"price\": 1.50e3,\n      \"name\": \"item  number {i}\",\n      \"note\": \"tab\\t and \\\"quote\\\" and \\u00e9\"\n    }},\n"
            ));
        }
        pretty.push_str("    null\n  ]\n}\n");
        let out = minify_json(&pretty).unwrap();
        assert!(out.starts_with("{\"items\":[{\"id\":0,\"price\":1.50e3,\"name\":\"item  number 0\",\"note\":\"tab\\t and \\\"quote\\\" and \\u00e9\"},"), "{out}");
        assert!(!out.contains('\n'));
        let reparsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed, serde_json::from_str::<serde_json::Value>(&pretty).unwrap());
        // NDJSON, broken JSON, scalars and plain text are refused.
        let ndjson = "{\"a\": 1}\n{\"a\": 2}\n".repeat(200);
        assert_eq!(minify_json(&ndjson), None);
        assert_eq!(minify_json(&pretty[..pretty.len() - 3]), None);
        assert_eq!(minify_json("\"just a string\""), None);
        assert_eq!(minify_json(&"not json at all\n".repeat(200)), None);
        // Already compact: nothing to gain.
        assert_eq!(minify_json(&out), None);
    }

    #[test]
    fn rtk_marks_are_recognised() {
        for text in [
            "lines here\n[+3 hidden: rtk recall f1d40b752001]\n",
            "[rtk: condensed 120 lines]",
            "... 40 lines omitted by rtk-proxy ...",
            "[full output: ~/.rtk/tee/1.log]",
            "see the full output in recall file]",
            "[full diff: rtk git diff --no-compact]",
        ] {
            assert!(condensed_upstream(text), "{text}");
        }
        for text in ["rtk recall abc", "the rtk crate", "full output follows", "[rtk]"] {
            assert!(!condensed_upstream(text), "{text}");
        }
    }

    #[test]
    fn inert_cleaning_changes_only_what_a_terminal_would_not_show() {
        assert_eq!(inert("plain text\nmore\n"), None);
        assert_eq!(
            inert("\u{1b}[32mok\u{1b}[0m  \nprogress 10%\rprogress 100%\n"),
            Some("ok\nprogress 100%\n".to_string())
        );
        let repeated = "retrying the connection to the database\n".repeat(5);
        assert_eq!(
            inert(&repeated),
            Some("retrying the connection to the database\nlge: the line above repeats 4 more times\n".to_string())
        );
        // The terminal part only: repeats and trailing blanks stay.
        assert_eq!(terminal_only(&repeated), None);
        assert_eq!(
            terminal_only("\u{1b}[1mbold\u{1b}[0m  \n"),
            Some("bold  \n".to_string())
        );
        assert_eq!(terminal_only("50%\r100%\n"), Some("100%\n".to_string()));
    }

    #[test]
    fn a_numbered_read_keeps_its_numbers_and_real_line_numbers_in_markers() {
        let (text, symbols) = python_file();
        let numbered: String = text
            .lines()
            .enumerate()
            .map(|(i, l)| format!("{:>6}\t{l}\n", i + 1))
            .collect();
        let out = skeleton(
            "svc/orders.py",
            &numbered,
            1,
            &symbols,
            &|s| s.name == "cancel_order",
            true,
        )
        .unwrap();
        assert!(out.contains("     3\tdef create_order(order_id):\n     4\t    \"\"\"Docs of create_order.\"\"\"\n      \t    # lge: 31 lines of function `create_order` left out; read them with: sed -n '5,35p' svc/orders.py\n"), "{out}");
        // A read from further down keeps the numbers it had.
        let tail: String = numbered.lines().skip(40).map(|l| format!("{l}\n")).collect();
        let out = skeleton("svc/orders.py", &tail, 41, &symbols, &|_| false, true).unwrap();
        assert!(
            out.contains("lines of function `refund_order` left out; read them with: sed -n '"),
            "{out}"
        );
        assert!(out.lines().nth(1).unwrap().starts_with("    41\t"), "{out}");
        // Numbers that do not match the range, or lines without them: nothing.
        assert_eq!(skeleton("svc/orders.py", &tail, 1, &symbols, &|_| false, true), None);
        assert_eq!(skeleton("svc/orders.py", &text, 1, &symbols, &|_| false, true), None);
    }
}
