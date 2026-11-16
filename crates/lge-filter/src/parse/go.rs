//! Go: `go test` (text and `-json`), `go build` and `go vet`.
//!
//! In text mode each failing test is a `--- FAIL: name (time)` line, subtests indented
//! under their parent; what the test logged is indented under it, or, with `-v`, was
//! printed before it under `=== RUN name`. Each package ends with `ok pkg`, `FAIL pkg`
//! or `? pkg [no test files]`. Compile and vet errors are `file.go:line:col: message`.
//!
//! In JSON mode (`go test -json`) every line is an event; a test's output is collected
//! from its `output` events and reported when its `fail` event comes.

use super::{closing_state, is_global_epilogue, number, pattern, Parsed};
use crate::format::{clip_lines, failure};
use crate::Failure;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};

// Lines that only say a test started, paused, went on or passed.
pattern!(PROGRESS, r"^\s*(?:=== (?:RUN|PAUSE|CONT|NAME)\b|--- (?:PASS|SKIP):)");
pattern!(RUN, r"^=== (RUN|PAUSE|CONT|NAME)\s+(\S+)");
pattern!(PASSED, r"^\s*--- (PASS|SKIP): (\S+)");
pattern!(FAILED, r"^(\s*)--- FAIL: (\S+)(?: \(.*\))?\s*$");
// What `t.Errorf` prints: `    cart_test.go:7: Total() = 3.01, want 3`.
pattern!(LOGGED, r"^\s+(\S+\.go):(\d+):\s?(.*)$");
pattern!(LOCATED, r"(\S+\.go):(\d+):\s?(.*)$");
pattern!(PANIC, r"^panic: (.*)$");
pattern!(GOROUTINE, r"^goroutine \d+ \[");
pattern!(HEADER, r"^# (\S+)");
pattern!(BUILD, r"^(vet: )?(\S+\.go):(\d+):(\d+): (.+)$");
// Package lines, in the shape `go test` prints them (a tab-separated time or a
// bracketed reason): jest prints `FAIL src/a.test.ts` too.
pattern!(OK, r"^ok\s+(\S+)\s+(?:\(cached\)|\d+(?:\.\d+)?s\b)");
pattern!(
    FAIL_PKG,
    r"^FAIL\s+(\S+)\s+(?:\d+(?:\.\d+)?s\b|\[(?:build|setup) failed\]|\(cached\))"
);
pattern!(NO_TESTS, r"^\?\s+(\S+)\s+\[no test files\]");
pattern!(REASON, r"\[([a-z ]+)\]\s*$");
// What `go test` prints after the last package line.
pattern!(EPILOGUE, r"^(?:FAIL|PASS)$");

/// Longest goroutine trace kept with a panic, in lines.
const GOROUTINE_LINES: usize = 20;
/// Longest continuation of a compile error kept (`have (int)`, `want (string)`).
const BUILD_DETAIL_LINES: usize = 6;

pub(crate) fn parse(text: &str) -> Parsed {
    let json = text
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .and_then(event)
        .is_some();
    if json {
        parse_json(text)
    } else {
        parse_text(text)
    }
}

/// Package counts for the summary.
#[derive(Default)]
struct Packages {
    ok: usize,
    failed: usize,
    without_tests: usize,
}

impl Packages {
    fn summary(&self) -> Option<String> {
        (self.ok + self.failed + self.without_tests > 0).then(|| {
            format!(
                "{} packages ok, {} failed, {} without test files",
                self.ok, self.failed, self.without_tests
            )
        })
    }
}

fn parse_text(text: &str) -> Parsed {
    let lines: Vec<&str> = text.lines().collect();
    let mut parsed = Parsed::default();
    let mut packages = Packages::default();
    let mut recognised = false;
    // The lines since the last package line, and how many failures were read in them.
    let (mut segment, mut segment_failures) = (0, 0);
    // The package a `# pkg` header names, and the packages whose build errors were read.
    let mut building: Option<&str> = None;
    let mut broken: BTreeSet<&str> = BTreeSet::new();
    // The test that is running (a panic is its), and the test whose `-v` output the
    // indented lines are.
    let (mut running, mut current): (Option<&str>, Option<&str>) = (None, None);
    let mut output: HashMap<&str, Vec<&str>> = HashMap::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if let Some(c) = RUN.captures(line) {
            let name = c.get(2).map_or("", |m| m.as_str());
            recognised |= &c[1] == "RUN";
            if &c[1] == "PAUSE" {
                current = None;
            } else {
                (running, current) = (Some(name), Some(name));
            }
            continue;
        }
        if let Some(c) = PASSED.captures(line) {
            recognised |= &c[1] == "PASS";
            if running == c.get(2).map(|m| m.as_str()) {
                running = None;
            }
            current = None;
            continue;
        }
        if let Some(c) = FAILED.captures(line) {
            recognised = true;
            let indent = c[1].len();
            let name = c.get(2).map_or("", |m| m.as_str());
            let block: Vec<&str> = lines[i..]
                .iter()
                .copied()
                .take_while(|l| !l.trim().is_empty() && indentation(l) > indent)
                .collect();
            parsed
                .failures
                .push(test_failure(name, &block, output.get(name).map(Vec::as_slice)));
            segment_failures += 1;
            if indent == 0 {
                running = Some(name);
            }
            current = None;
            // Subtests are failures of their own: the lines under this one are read again.
            continue;
        }
        if let Some(c) = PANIC.captures(line) {
            let name = running.map_or_else(|| "panic".to_string(), |t| format!("panic in {t}"));
            let (excerpt, end) = panic_report(&lines, i);
            i = end;
            parsed.failures.push(failure("", None, &name, &c[1], &excerpt));
            segment_failures += 1;
            current = None;
            continue;
        }
        if let Some(c) = HEADER.captures(line) {
            building = c.get(1).map(|m| m.as_str());
            continue;
        }
        if let Some(c) = BUILD.captures(line) {
            let detail = build_detail(&lines, &mut i);
            parsed.failures.push(build_failure(&c, &detail));
            segment_failures += 1;
            broken.extend(building);
            continue;
        }
        let package = if OK.is_match(line) {
            packages.ok += 1;
            true
        } else if let Some(c) = FAIL_PKG.captures(line) {
            packages.failed += 1;
            let pkg = c.get(1).map_or("", |m| m.as_str());
            if segment_failures == 0 && !broken.contains(pkg) {
                // Nothing said why: the package's own lines are the reason.
                let excerpt: Vec<String> = lines[segment..i - 1]
                    .iter()
                    .filter(|l| !l.trim().is_empty() && !PROGRESS.is_match(l) && !EPILOGUE.is_match(l))
                    .map(|l| l.to_string())
                    .collect();
                parsed.failures.push(failure(
                    "",
                    None,
                    &format!("FAIL {pkg}"),
                    reason(line),
                    &clip_lines(excerpt, 10, 10),
                ));
            }
            true
        } else if NO_TESTS.is_match(line) {
            packages.without_tests += 1;
            true
        } else {
            false
        };
        if package {
            recognised = true;
            (segment, segment_failures) = (i, 0);
            (running, current, building) = (None, None, None);
            continue;
        }
        if let Some(name) = current.filter(|_| line.starts_with(char::is_whitespace)) {
            output.entry(name).or_default().push(line);
        }
    }
    parsed.recognised = recognised;
    parsed.summary = packages.summary();
    (parsed.closed, parsed.trailing) = closing_state(text, &[&OK, &FAIL_PKG, &NO_TESTS], &[&EPILOGUE]);
    parsed
}

/// Leading spaces and tabs.
fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// The bracketed reason of a `FAIL pkg [build failed]` line, or nothing (a time).
fn reason(line: &str) -> &str {
    REASON.captures(line).and_then(|c| c.get(1)).map_or("", |m| m.as_str())
}

/// One failing test. `block` is what is indented under its `--- FAIL` line; `before`
/// is what `-v` printed under its `=== RUN` line, used when the block names no place.
fn test_failure(name: &str, block: &[&str], before: Option<&[&str]>) -> Failure {
    let block: Vec<&str> = block.iter().copied().filter(|l| !PROGRESS.is_match(l)).collect();
    let lines: Vec<&str> = if own_location(&block).is_some() {
        block
    } else {
        before.unwrap_or_default().iter().copied().chain(block).collect()
    };
    let at = own_location(&lines);
    let (file, line, message) = match at.and_then(|at| LOGGED.captures(lines[at])) {
        Some(c) => (
            c.get(1).map_or("", |m| m.as_str()),
            number(&c[2]),
            c.get(3).map_or("", |m| m.as_str()),
        ),
        None => ("", None, ""),
    };
    let excerpt: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != at)
        .map(|(_, l)| l.to_string())
        .collect();
    failure(file, line, name, message, &excerpt)
}

/// The first logged location of the test itself, before any subtest's `--- FAIL`.
fn own_location(lines: &[&str]) -> Option<usize> {
    lines
        .iter()
        .take_while(|l| !FAILED.is_match(l))
        .position(|l| LOGGED.is_match(l))
}

/// What follows a `panic:` line: a repeated panic or a signal line, then the next
/// goroutine block, cut at [`GOROUTINE_LINES`]. Returns the lines and where reading
/// goes on.
fn panic_report(lines: &[&str], from: usize) -> (Vec<String>, usize) {
    let near = (from + 6).min(lines.len());
    let Some(header) = (from..near).find(|&k| GOROUTINE.is_match(lines[k])) else {
        return (Vec::new(), from);
    };
    let mut excerpt: Vec<String> = lines[from..header]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();
    let mut i = header;
    while i < lines.len() && !lines[i].trim().is_empty() && !is_package_line(lines[i]) {
        if i - header < GOROUTINE_LINES {
            excerpt.push(lines[i].to_string());
        }
        i += 1;
    }
    (excerpt, i)
}

fn is_package_line(line: &str) -> bool {
    OK.is_match(line) || FAIL_PKG.is_match(line) || NO_TESTS.is_match(line) || EPILOGUE.is_match(line)
}

/// The tab-indented lines under a compile error.
fn build_detail(lines: &[&str], i: &mut usize) -> Vec<String> {
    let mut detail = Vec::new();
    while *i < lines.len() && lines[*i].starts_with('\t') {
        if detail.len() < BUILD_DETAIL_LINES {
            detail.push(lines[*i][1..].to_string());
        }
        *i += 1;
    }
    detail
}

fn build_failure(c: &regex::Captures, detail: &[String]) -> Failure {
    let name = if c.get(1).is_some() { "vet" } else { "build" };
    failure(&c[2], number(&c[3]), name, &c[5], detail)
}

/// A `go test -json` event: an object with an `Action`.
fn event(line: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(line.trim()) {
        Ok(Value::Object(map)) if map.get("Action").is_some_and(Value::is_string) => Some(map),
        _ => None,
    }
}

fn field<'a>(map: &'a Map<String, Value>, key: &str) -> &'a str {
    map.get(key).and_then(Value::as_str).unwrap_or("")
}

fn parse_json(text: &str) -> Parsed {
    let lines: Vec<&str> = text.lines().collect();
    let mut parsed = Parsed {
        recognised: true,
        ..Parsed::default()
    };
    let mut packages = Packages::default();
    // Output per (package, test); the package's own output has an empty test.
    let mut output: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut failed_tests: Vec<(String, String)> = Vec::new();
    // Failed packages, and whether the event says their build failed.
    let mut failed_packages: Vec<(String, bool)> = Vec::new();
    // Build output per import path, in the order the builds reported.
    let mut builds: Vec<(String, Vec<String>)> = Vec::new();
    let mut build_failed: Vec<String> = Vec::new();
    // Lines that are not events: build errors an older `go` printed to stderr.
    let mut plain: Vec<&str> = Vec::new();
    let mut closing: Option<usize> = None;

    for (at, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Some(e) = event(line) else {
            plain.push(line);
            continue;
        };
        let (package, test) = (field(&e, "Package").to_string(), field(&e, "Test").to_string());
        match field(&e, "Action") {
            "output" => {
                let text = field(&e, "Output");
                let lines = output.entry((package, test)).or_default();
                lines.extend(text.strip_suffix('\n').unwrap_or(text).split('\n').map(str::to_string));
            }
            "fail" if !test.is_empty() => failed_tests.push((package, test)),
            "fail" => {
                packages.failed += 1;
                failed_packages.push((package, e.contains_key("FailedBuild")));
                closing = Some(at);
            }
            "pass" if test.is_empty() => {
                packages.ok += 1;
                closing = Some(at);
            }
            "skip" if test.is_empty() => {
                packages.without_tests += 1;
                closing = Some(at);
            }
            "build-output" => {
                let import = first_word(field(&e, "ImportPath")).to_string();
                let text = field(&e, "Output");
                let new: Vec<String> = text
                    .strip_suffix('\n')
                    .unwrap_or(text)
                    .split('\n')
                    .map(str::to_string)
                    .collect();
                match builds.iter_mut().find(|(i, _)| *i == import) {
                    Some((_, lines)) => lines.extend(new),
                    None => builds.push((import, new)),
                }
            }
            "build-fail" => build_failed.push(first_word(field(&e, "ImportPath")).to_string()),
            _ => {}
        }
    }

    // Compile errors, from build events and from plain lines.
    let mut broken: BTreeSet<String> = BTreeSet::new();
    for (import, lines) in &builds {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        if read_build_errors(&refs, &mut parsed.failures) > 0 {
            broken.insert(import.clone());
        }
    }
    for import in &build_failed {
        if !broken.contains(import) {
            let lines = builds
                .iter()
                .find(|(i, _)| i == import)
                .map(|(_, l)| l.clone())
                .unwrap_or_default();
            parsed.failures.push(failure("", None, "build", "build failed", &lines));
            broken.insert(import.clone());
        }
    }
    let before = parsed.failures.len();
    read_build_errors(&plain, &mut parsed.failures);
    let mut header: Option<&str> = None;
    for line in &plain {
        if let Some(c) = HEADER.captures(line) {
            header = c.get(1).map(|m| m.as_str());
        } else if BUILD.is_match(line) {
            broken.extend(header.map(str::to_string));
        }
    }
    let build_errors = !broken.is_empty() || parsed.failures.len() > before;

    for (package, test) in &failed_tests {
        let lines = output
            .get(&(package.clone(), test.clone()))
            .cloned()
            .unwrap_or_default();
        parsed.failures.push(json_test_failure(test, &lines));
    }
    for (package, failed_build) in &failed_packages {
        let has_test = failed_tests.iter().any(|(p, _)| p == package);
        // A build failure whose errors were read, even without a `# pkg` to say whose.
        if has_test || broken.contains(package) || (*failed_build && build_errors) {
            continue;
        }
        let lines = output
            .get(&(package.clone(), String::new()))
            .cloned()
            .unwrap_or_default();
        let fail_line = lines.iter().find(|l| FAIL_PKG.is_match(l)).cloned().unwrap_or_default();
        let excerpt: Vec<String> = lines
            .into_iter()
            .filter(|l| !l.trim().is_empty() && !EPILOGUE.is_match(l) && !FAIL_PKG.is_match(l))
            .collect();
        parsed.failures.push(failure(
            "",
            None,
            &format!("FAIL {package}"),
            reason(&fail_line),
            &clip_lines(excerpt, 10, 10),
        ));
    }

    parsed.summary = packages.summary();
    if let Some(at) = closing {
        parsed.closed = true;
        parsed.trailing = lines[at + 1..]
            .iter()
            .any(|l| !l.trim().is_empty() && event(l).is_none() && !is_global_epilogue(l));
    }
    parsed
}

/// `example.com/a [example.com/a.test]` is the package `example.com/a`.
fn first_word(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or("")
}

/// Adds one failure per compile or vet error in `lines`; returns how many.
fn read_build_errors(lines: &[&str], failures: &mut Vec<Failure>) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if let Some(c) = BUILD.captures(line) {
            let detail = build_detail(lines, &mut i);
            failures.push(build_failure(&c, &detail));
            count += 1;
        }
    }
    count
}

/// A failing test of `go test -json`: its output without the progress lines, the
/// place from the first `file.go:line:` in it, a goroutine trace cut short.
fn json_test_failure(test: &str, output: &[String]) -> Failure {
    let mut lines: Vec<&str> = Vec::new();
    let mut in_trace = 0;
    for line in output {
        if PROGRESS.is_match(line) || FAILED.is_match(line) {
            continue;
        }
        if GOROUTINE.is_match(line) {
            in_trace = 1;
        } else if in_trace > 0 && line.trim().is_empty() {
            in_trace = 0;
        } else if in_trace > 0 {
            in_trace += 1;
        }
        if in_trace <= GOROUTINE_LINES {
            lines.push(line);
        }
    }
    let at = lines.iter().position(|l| LOCATED.is_match(l));
    let (file, line, message) = match at.and_then(|at| LOCATED.captures(lines[at])) {
        Some(c) => (
            c.get(1).map_or("", |m| m.as_str()).to_string(),
            number(&c[2]),
            c.get(3).map_or("", |m| m.as_str()).to_string(),
        ),
        None => (
            String::new(),
            None,
            lines
                .iter()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .to_string(),
        ),
    };
    let excerpt: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != at)
        .map(|(_, l)| l.to_string())
        .collect();
    failure(&file, line, test, &message, &excerpt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{filter_for_proxy, filter_output, filter_with, FilterOptions};

    // Captured from go 1.27.4; the module path was shortened to /home/dev/shop and
    // GOROOT to /usr/local/go.
    const FAIL: &str = include_str!("../../testdata/go_fail.txt");
    const FAIL_V: &str = include_str!("../../testdata/go_fail_v.txt");
    const FAIL_JSON: &str = include_str!("../../testdata/go_fail_json.txt");
    const PASS: &str = include_str!("../../testdata/go_pass.txt");
    const PASS_JSON: &str = include_str!("../../testdata/go_pass_json.txt");
    const VET: &str = include_str!("../../testdata/go_vet.txt");
    const BUILD_ERR: &str = include_str!("../../testdata/go_build.txt");

    fn by_name<'a>(failures: &'a [Failure], name: &str) -> &'a Failure {
        failures
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no failure named {name} in {failures:#?}"))
    }

    fn names(failures: &[Failure]) -> Vec<&str> {
        let mut names: Vec<&str> = failures.iter().map(|f| f.name.as_str()).collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn a_passing_run_is_one_line_and_closed() {
        assert_eq!(
            filter_output("go", PASS),
            (
                Vec::new(),
                "ok: 1 packages ok, 0 failed, 1 without test files".to_string()
            )
        );
        let parsed = parse(PASS);
        assert!(parsed.recognised && parsed.closed && !parsed.trailing);
        assert_eq!(
            filter_output("go", PASS_JSON),
            (
                Vec::new(),
                "ok: 1 packages ok, 0 failed, 1 without test files".to_string()
            )
        );
        let parsed = parse(PASS_JSON);
        assert!(parsed.recognised && parsed.closed && !parsed.trailing);
    }

    #[test]
    fn text_failures_build_errors_and_a_panic() {
        let parsed = parse(FAIL);
        assert!(parsed.recognised && parsed.closed && !parsed.trailing);
        assert_eq!(
            parsed.summary.as_deref(),
            Some("1 packages ok, 3 failed, 1 without test files")
        );
        assert_eq!(
            names(&parsed.failures),
            [
                "TestEmpty",
                "TestPanics",
                "TestTable",
                "TestTable/one",
                "TestTotal",
                "build",
                "build",
                "build",
                "panic in TestPanics"
            ]
        );
        let total = by_name(&parsed.failures, "TestTotal");
        assert_eq!((total.file.as_str(), total.line), ("cart_test.go", Some(7)));
        assert_eq!(total.message, "Total() = 3.01, want 3");
        assert_eq!(total.excerpt, "");
        let empty = by_name(&parsed.failures, "TestEmpty");
        assert_eq!(empty.excerpt, "second line of the message");
        let sub = by_name(&parsed.failures, "TestTable/one");
        assert_eq!((sub.file.as_str(), sub.line), ("cart_test.go", Some(26)));
        assert_eq!(sub.message, "got 1.01, want 1");
        // The parent has no message of its own; its block holds the subtest's lines.
        let parent = by_name(&parsed.failures, "TestTable");
        assert_eq!((parent.file.as_str(), parent.message.as_str()), ("", ""));
        assert!(parent.excerpt.contains("--- FAIL: TestTable/one"));

        let panic = by_name(&parsed.failures, "panic in TestPanics");
        assert_eq!(
            panic.message,
            "runtime error: index out of range [5] with length 3 [recovered, repanicked]"
        );
        assert!(panic.excerpt.starts_with("goroutine 35 [running]:"));
        assert!(panic.excerpt.contains("pricing_test.go:8"));
        assert!(panic.excerpt.lines().count() <= GOROUTINE_LINES);

        let builds: Vec<(&str, Option<u32>)> = parsed
            .failures
            .iter()
            .filter(|f| f.name == "build")
            .map(|f| (f.file.as_str(), f.line))
            .collect();
        assert_eq!(
            builds,
            [
                ("broken/broken.go", Some(4)),
                ("broken/broken.go", Some(8)),
                ("broken/broken.go", Some(9))
            ]
        );
        // The packages that failed are explained by their failures: no `FAIL pkg` entry.
        assert!(!parsed.failures.iter().any(|f| f.name.starts_with("FAIL ")));
    }

    #[test]
    fn verbose_output_before_the_fail_line_is_the_tests() {
        let parsed = parse(FAIL_V);
        assert!(parsed.recognised && parsed.closed && !parsed.trailing);
        let total = by_name(&parsed.failures, "TestTotal");
        assert_eq!((total.file.as_str(), total.line), ("cart_test.go", Some(7)));
        assert_eq!(total.message, "Total() = 3.01, want 3");
        let empty = by_name(&parsed.failures, "TestEmpty");
        assert_eq!(empty.excerpt, "second line of the message");
        let sub = by_name(&parsed.failures, "TestTable/one");
        assert_eq!((sub.line, sub.message.as_str()), (Some(26), "got 1.01, want 1"));
        let parent = by_name(&parsed.failures, "TestTable");
        assert!(!parent.excerpt.contains("PASS"), "{}", parent.excerpt);
        assert!(parsed.failures.iter().any(|f| f.name == "panic in TestPanics"));
        assert_eq!(
            parsed.summary.as_deref(),
            Some("1 packages ok, 2 failed, 1 without test files")
        );
    }

    #[test]
    fn json_events_give_the_same_failures() {
        let parsed = parse(FAIL_JSON);
        assert!(parsed.recognised && parsed.closed && !parsed.trailing);
        assert_eq!(
            parsed.summary.as_deref(),
            Some("1 packages ok, 3 failed, 1 without test files")
        );
        assert_eq!(
            names(&parsed.failures),
            [
                "TestEmpty",
                "TestPanics",
                "TestTable",
                "TestTable/one",
                "TestTotal",
                "build",
                "build",
                "build"
            ]
        );
        let total = by_name(&parsed.failures, "TestTotal");
        assert_eq!((total.file.as_str(), total.line), ("cart_test.go", Some(7)));
        assert_eq!(total.message, "Total() = 3.01, want 3");
        let empty = by_name(&parsed.failures, "TestEmpty");
        assert_eq!(empty.excerpt, "second line of the message");
        let panic = by_name(&parsed.failures, "TestPanics");
        assert!(panic.message.starts_with("panic: runtime error: index out of range"));
        assert!(panic.excerpt.contains("goroutine 35 [running]:"));
        let builds: Vec<Option<u32>> = parsed
            .failures
            .iter()
            .filter(|f| f.name == "build")
            .map(|f| f.line)
            .collect();
        assert_eq!(builds, [Some(4), Some(8), Some(9)]);
    }

    #[test]
    fn json_package_failure_without_a_test_and_trailing_text() {
        let raw = "{\"Action\":\"start\",\"Package\":\"example.com/a\"}\n\
                   {\"Action\":\"output\",\"Package\":\"example.com/a\",\"Output\":\"TestMain: setup failed: no database\\n\"}\n\
                   {\"Action\":\"output\",\"Package\":\"example.com/a\",\"Output\":\"FAIL\\texample.com/a\\t0.01s\\n\"}\n\
                   {\"Action\":\"fail\",\"Package\":\"example.com/a\",\"Elapsed\":0.01}\n\
                   report: 120 rows written\n";
        let parsed = parse(raw);
        assert_eq!(names(&parsed.failures), ["FAIL example.com/a"]);
        assert_eq!(parsed.failures[0].excerpt, "TestMain: setup failed: no database");
        assert!(parsed.closed && parsed.trailing);
        // Cut before the package's own event: never closed.
        let cut: String = raw.lines().take(3).map(|l| format!("{l}\n")).collect();
        assert!(!parse(&cut).closed);
    }

    #[test]
    fn a_package_that_fails_without_a_failed_test_is_named() {
        let raw = "ok  \texample.com/a\t0.01s\nsetup: cannot reach the database\nexit status 1\nFAIL\texample.com/b\t0.02s\nFAIL\n";
        let parsed = parse(raw);
        assert_eq!(names(&parsed.failures), ["FAIL example.com/b"]);
        assert_eq!(
            parsed.failures[0].excerpt,
            "setup: cannot reach the database\nexit status 1"
        );
        assert!(parsed.closed && !parsed.trailing);
    }

    #[test]
    fn a_run_cut_short_or_followed_by_another_program() {
        let cut = "=== RUN   TestA\n--- PASS: TestA (0.00s)\n=== RUN   TestB\n";
        let parsed = parse(cut);
        assert!(parsed.recognised && !parsed.closed && parsed.failures.is_empty());
        let after = format!("{PASS}starting the report server on :8080\n");
        let parsed = parse(&after);
        assert!(parsed.closed && parsed.trailing);
        let make = format!("{FAIL}make: *** [Makefile:3: test] Error 1\n");
        assert!(!parse(&make).trailing);
    }

    #[test]
    fn vet_and_build_output() {
        let (failures, text) = filter_output("go", VET);
        assert_eq!(failures.len(), 2);
        assert!(failures.iter().all(|f| f.name == "build" && f.file == "vetme/v.go"));
        assert!(
            text.starts_with("vetme/v.go\n  6: build: fmt.Sprintf format %d has arg \"three\" of wrong type string\n")
        );
        let parsed = parse(VET);
        assert!(!parsed.recognised && !parsed.closed);
        let prefixed = parse("# example.com/a\nvet: ./a.go:3:2: unreachable code\n");
        assert_eq!(prefixed.failures[0].name, "vet");
        assert_eq!(prefixed.failures[0].file, "./a.go");

        let (failures, _) = filter_output("go build ./...", BUILD_ERR);
        assert_eq!(failures.len(), 3);
        let detail =
            parse("x.go:3:9: cannot use x (variable of type int) as string value\n\thave int\n\twant string\nnext\n");
        assert_eq!(detail.failures[0].excerpt, "have int\nwant string");
    }

    #[test]
    fn jest_suite_lines_are_not_go_package_lines() {
        for line in ["FAIL src/cart.test.ts", "FAIL src/cart.test.ts (5.123 s)", "ok 1 - a"] {
            let parsed = parse(&format!("{line}\n"));
            assert!(!parsed.recognised && !parsed.closed, "{line}");
        }
    }

    #[test]
    fn many_failing_tests_are_all_named() {
        let mut raw = String::new();
        for i in 0..3000 {
            raw.push_str(&format!("=== RUN   TestOk{i}\n--- PASS: TestOk{i} (0.00s)\n"));
            if i % 10 == 0 {
                raw.push_str(&format!(
                    "=== RUN   TestBad{i}\n    x_test.go:{i}: got 1, want 2\n--- FAIL: TestBad{i} (0.00s)\n"
                ));
            }
        }
        raw.push_str("FAIL\nFAIL\texample.com/shop\t0.512s\nFAIL\n");
        let failed = FilterOptions {
            passed: Some(false),
            ..FilterOptions::default()
        };
        let (failures, text) = filter_with("go test ./...", &raw, &failed);
        assert_eq!(failures.len(), 300);
        for i in (0..3000).step_by(10) {
            assert!(text.contains(&format!("{i}: TestBad{i}")), "lost TestBad{i}");
        }
        assert!(text.len() < raw.len() / 5);
        let proxy = filter_for_proxy("go", &raw, &failed);
        assert!(proxy.closed && !proxy.trailing && proxy.recognised);
    }
}
