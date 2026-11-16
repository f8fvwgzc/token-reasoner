//! cargo: compiler and clippy diagnostics (`error[E0308]: ...` with a ` --> file:line:col`
//! location) and test results (`---- name stdout ----` blocks, the `failures:` list and
//! `test result:` lines). `cargo test` prints both, so they are one parser.
//! `cargo nextest run` prints the same diagnostics, then one status line per test and
//! the captured output of each failing one.

use super::{closing_state, count_of, number, pattern, Parsed};
use crate::format::{clip_lines, failure};
use crate::Failure;
use regex::Regex;
use std::collections::BTreeMap;

pattern!(DIAG, r"^(error|warning)(?:\[(E\d+)\])?: (.*)$");
pattern!(ARROW, r"^\s*--> (.+?):(\d+):(\d+)$");
// Lines cargo prints with the same prefix that only count or repeat the diagnostics above them.
pattern!(
    NOT_A_DIAGNOSTIC,
    r"^(?:could not compile|aborting due to|test failed, to rerun|doctest failed, to rerun|\d+ targets? failed|build failed|`[^`]+` \([^)]*\) generated \d+ warnings?|unused manifest key)"
);
pattern!(CLIPPY_LINT, r"rust-clippy/[^\s#]*#([a-z0-9_]+)");
pattern!(IMPLIED_LINT, r"`-[DWF] ([a-z0-9_:-]+)` implied by");
pattern!(LEVEL_LINT, r"#\[(?:warn|deny|forbid)\(([a-z0-9_:]+)\)\]");
pattern!(ALLOW_LINT, r"#\[allow\(([a-z0-9_:]+)\)\]");
// Says which lint fired or how to silence it; the lint name is already the failure's name.
pattern!(
    LINT_NOTE,
    r"^\s*= (?:help: for further information visit|help: to override|note: `-[DWF] |note: `#\[(?:warn|deny|forbid)\()"
);
// cargo right-aligns its status word in twelve columns; the exact shape keeps other
// tools' "Running ..." lines from looking like cargo.
pattern!(
    CARGO_STATUS,
    r"^(?:   Compiling|    Checking|    Finished|     Running|   Doc-tests| Documenting|       Fresh) "
);
pattern!(RUNNING, r"^running \d+ tests?$");
pattern!(TEST_LINE, r"^test (.+?) \.\.\. (.*)$");
pattern!(VERDICT, r"^(?:ok|FAILED|ignored|bench)\b");
pattern!(STDOUT_HEAD, r"^---- (.+) (?:stdout|stderr) ----$");
pattern!(
    RESULT,
    r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored;"
);
// `thread 'a::b' (1234) panicked at src/cart.rs:31:9:` and, before Rust 1.73,
// `thread 'a::b' panicked at 'message', src/cart.rs:31:9`.
pattern!(PANIC_AT, r"panicked at (.+?):(\d+):(\d+):?$");
pattern!(OLD_PANIC, r"panicked at '(.*)', (.+?):(\d+):(\d+)$");
pattern!(
    NO_PANIC,
    r"^note: (test did not panic as expected)(?: at (.+?):(\d+):(\d+))?"
);
// The thread id changes on every run; the same failure should give the same bytes.
pattern!(THREAD_ID, r"^(thread '.*') \(\d+\) panicked");
// A snippet line with nothing but the gutter bar.
pattern!(EMPTY_GUTTER, r"^\s*\|\s*$");
// A doc test is named after its file and line: `src/lib.rs - add_one (line 6)`.
pattern!(DOCTEST, r"^(\S+) - .*\(line (\d+)\)$");

// The lines a cargo run closes with: a test binary's result, the build's `Finished`,
// nextest's `Summary`.
pattern!(CLOSING, r"^(?:test result: |\s*Finished |\s*Summary \[)");
// What cargo prints after a failed test run, down to the list of targets to rerun
// (`` `--lib` `` in a single package, `` `-p shop --lib` `` in a workspace).
pattern!(
    EPILOGUE,
    r"^(?:error: (?:test|doctest) failed, to rerun pass|error: \d+ targets? failed:|\s+`-)"
);

// nextest: `        FAIL [   0.005s] shop cart::tests::totals_items`, with a
// `(1/3)` counter in newer versions.
pattern!(
    NEXTEST_FAIL,
    r"^\s*(?:FAIL|SIGSEGV|SIGABRT|SIGBUS|SIGILL|TIMEOUT|LEAK|LEAK-FAIL) \[\s*[\d.]+s\]\s+(?:\(\s*\d+/\d+\)\s+)?(\S+) (\S+)\s*$"
);
pattern!(
    NEXTEST_STATUS,
    r"^\s*(?:PASS|FAIL|SIGSEGV|SIGABRT|SIGBUS|SIGILL|TIMEOUT|LEAK|LEAK-FAIL|FLAKY|SLOW|SKIP|TRY \d+ \w+|RETRY|START|DELAY) \[\s*[\d.]+s\]"
);
pattern!(NEXTEST_PASS, r"^\s*PASS \[\s*[\d.]+s\]");
pattern!(
    NEXTEST_SUMMARY,
    r"^\s*Summary \[\s*[\d.]+s\] (\d+ tests? run: \d+ passed.*?)\s*$"
);
// `--- STDOUT:              shop cart::tests::totals_items ---`, `──── STDERR: shop a::b`
pattern!(
    NEXTEST_OUTPUT,
    r"^\s*(?:-{3,}|─{2,}) ?(?:STDOUT|STDERR|OUTPUT):\s+(\S+) (\S+)(?:\s+[-─]+)?\s*$"
);
pattern!(NEXTEST_RULE, r"^\s*-{12,}\s*$");

/// One test binary's run, from `running N tests` to `test result:`.
#[derive(Default)]
struct TestRun<'a> {
    /// The `failures:` list at the end: the authoritative names.
    names: Vec<&'a str>,
    /// Tests the progress lines marked `FAILED`.
    failed: Vec<&'a str>,
    /// The test whose progress line has no verdict: it was running when the binary died.
    unfinished: Option<&'a str>,
    blocks: BTreeMap<&'a str, Vec<&'a str>>,
    block_order: Vec<&'a str>,
    in_failures: bool,
    block: Option<&'a str>,
    /// Index of the `failures:` line that opens the list of names, when looking ahead
    /// found it: every line before it is captured output, whatever it looks like.
    list_at: Option<usize>,
}

/// Whether `name` is one of the tests the progress lines marked `FAILED`. A doc test
/// carries its mode there (`... (line 5) - compile fail`) and nowhere else.
fn is_failed(failed: &[&str], name: &str) -> bool {
    failed
        .iter()
        .any(|f| *f == name || f.strip_prefix(name).is_some_and(|mode| mode.starts_with(" - ")))
}

/// Where the list of failed names starts in `lines`: the first `failures:` line
/// followed by the name of a test that did fail. A test can print lines that look like
/// the harness's own (a tool that runs `cargo test`, for one); its captured output can
/// hold a `failures:` list and a `test result:` line, but not with this run's names.
///
/// `budget` bounds the lines looked at over a whole parse, so output made of thousands
/// of cut-off runs is still read in linear time.
fn names_list_at(lines: &[&str], failed: &[&str], budget: &mut usize) -> Option<usize> {
    if failed.is_empty() {
        return None;
    }
    for (at, line) in lines.iter().enumerate() {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let names = lines[at + 1..].iter().take_while(|l| l.starts_with("    "));
        if *line == "failures:" && names.into_iter().any(|l| is_failed(failed, l.trim())) {
            return Some(at);
        }
    }
    None
}

impl<'a> TestRun<'a> {
    fn failures(&self, finished: bool, caused_by: Option<&str>) -> Vec<Failure> {
        let mut names: Vec<&str> = self.names.clone();
        if names.is_empty() {
            // A run that was cut short has no list: fall back to what was seen.
            for name in self.failed.iter().chain(&self.block_order) {
                if !names.contains(name) {
                    names.push(*name);
                }
            }
        }
        let mut out: Vec<Failure> = names
            .iter()
            .map(|name| test_failure(name, self.blocks.get(name).map_or(&[][..], |b| b.as_slice())))
            .collect();
        if !finished {
            let name = self.unfinished.unwrap_or("(test binary)");
            let message = caused_by.unwrap_or("the test run did not finish");
            out.push(failure("", None, name, message, &[]));
        }
        out
    }
}

pub(crate) fn parse(text: &str) -> Parsed {
    let lines: Vec<&str> = text.lines().collect();
    let mut parsed = Parsed::default();
    // nextest's lines, and the output it captured, are read apart from the rest: a
    // captured `test result:` line is a test's output, not a binary of this run.
    let nextest = nextest(&lines);
    let mut run: Option<TestRun> = None;
    let (mut passed, mut failed, mut ignored, mut results) = (0usize, 0usize, 0usize, 0usize);
    let caused_by = caused_by(&lines);
    let mut lookahead = lines.len().saturating_mul(4);
    // A bare `error: ...` was taken as a failure, and a line nothing here reads was seen.
    let (mut weak, mut loose) = (false, false);

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if nextest.as_ref().is_some_and(|n| n.consumed[i - 1]) {
            continue;
        }

        // Before the list of names, the failures section is captured output only.
        if let Some(r) = run.as_mut().filter(|r| r.list_at.is_some_and(|at| i <= at)) {
            let header = STDOUT_HEAD.captures(line).and_then(|c| c.get(1));
            if let Some(name) = header.map(|m| m.as_str()).filter(|name| is_failed(&r.failed, name)) {
                r.block = Some(name);
                r.block_order.push(name);
                r.blocks.entry(name).or_default();
            } else if let Some(name) = r.block {
                r.blocks.entry(name).or_default().push(line);
            }
            continue;
        }

        // Inside the failures section every line belongs to a test's captured output,
        // even one that happens to look like cargo's own status lines.
        let in_output = run.as_ref().is_some_and(|r| r.in_failures);
        if !in_output && (RUNNING.is_match(line) || CARGO_STATUS.is_match(line)) {
            parsed.recognised = true;
            // A new binary starts while the last one never reported: it crashed.
            if let Some(r) = run.take() {
                parsed.failures.extend(r.failures(false, caused_by));
            }
            if RUNNING.is_match(line) {
                run = Some(TestRun::default());
            }
            continue;
        }
        if let Some(c) = RESULT.captures(line) {
            parsed.recognised = true;
            results += 1;
            passed += c[1].parse::<usize>().unwrap_or(0);
            failed += c[2].parse::<usize>().unwrap_or(0);
            ignored += c[3].parse::<usize>().unwrap_or(0);
            if let Some(r) = run.take() {
                parsed.failures.extend(r.failures(true, None));
            }
            continue;
        }
        if let Some(r) = run.as_mut() {
            if line == "failures:" {
                if !r.in_failures {
                    r.list_at = names_list_at(&lines[i..], &r.failed, &mut lookahead).map(|at| i + at);
                }
                r.in_failures = true;
                r.block = None;
                continue;
            }
            if r.in_failures {
                if let Some(c) = STDOUT_HEAD.captures(line) {
                    let name = c.get(1).map_or("", |m| m.as_str());
                    r.block = Some(name);
                    r.block_order.push(name);
                    r.blocks.entry(name).or_default();
                } else if let Some(name) = r.block {
                    r.blocks.entry(name).or_default().push(line);
                } else if let Some(name) = line.strip_prefix("    ") {
                    r.names.push(name.trim());
                }
                continue;
            }
            if let Some(c) = TEST_LINE.captures(line) {
                let name = c.get(1).map_or("", |m| m.as_str());
                let name = name.strip_suffix(" - should panic").unwrap_or(name);
                let verdict = c.get(2).map_or("", |m| m.as_str());
                if verdict.starts_with("FAILED") {
                    r.failed.push(name);
                } else if !VERDICT.is_match(verdict) {
                    r.unfinished = Some(name);
                }
                continue;
            }
        }
        if let Some(c) = DIAG.captures(line) {
            // A diagnostic runs to the next blank line or the next diagnostic.
            let start = i;
            while i < lines.len()
                && !lines[i].trim().is_empty()
                && !DIAG.is_match(lines[i])
                && !CARGO_STATUS.is_match(lines[i])
            {
                i += 1;
            }
            let message = c.get(3).map_or("", |m| m.as_str());
            if NOT_A_DIAGNOSTIC.is_match(message) {
                continue;
            }
            let (strong, f) = diagnostic(
                c.get(2).map(|m| m.as_str()),
                &c[1],
                message,
                &lines[start..i],
                &lines[i..],
            );
            parsed.recognised |= strong;
            let list = if &c[1] == "warning" {
                &mut parsed.warnings
            } else {
                weak |= !strong;
                &mut parsed.failures
            };
            // The same diagnostic is printed once per target (lib, lib test).
            if !list.contains(&f) {
                list.push(f);
            }
        } else if line.trim() == "Caused by:" {
            // cargo's explanation of the error above it, which `diagnostic` has taken.
            while i < lines.len() && !lines[i].trim().is_empty() {
                i += 1;
            }
        } else if run.is_none() && !line.trim().is_empty() {
            loose = true;
        }
    }
    if let Some(r) = run.take() {
        parsed.failures.extend(r.failures(false, caused_by));
    }
    // `error: test run failed` is all another runner behind `cargo` (nextest, an xtask)
    // says in cargo's words: the failing tests are in lines this parser does not read.
    parsed.partial = weak && loose;

    if results > 0 {
        parsed.reported = Some(failed);
        parsed.summary = Some(format!("{passed} passed; {failed} failed; {ignored} ignored"));
    } else if !parsed.warnings.is_empty() {
        parsed.summary = Some(format!("{} warnings", parsed.warnings.len()));
    }
    parsed.suites = results;
    if let Some(n) = nextest {
        n.apply(&mut parsed);
    }
    (parsed.closed, parsed.trailing) = closing_state(text, &[&CLOSING], &[&EPILOGUE]);
    parsed
}

/// What a nextest run printed: one status line per test, the output it captured for
/// each failing test, and a `Summary` line.
struct Nextest<'a> {
    /// Lines read here, which the rest of the parser must not read again.
    consumed: Vec<bool>,
    /// Failing tests as `binary test`, in the order they finished, with their output.
    failed: Vec<(String, Vec<&'a str>)>,
    passed: usize,
    summary: Option<&'a str>,
}

/// Reads nextest's lines, or `None` when the output has none.
fn nextest<'a>(lines: &[&'a str]) -> Option<Nextest<'a>> {
    if !lines
        .iter()
        .any(|l| NEXTEST_STATUS.is_match(l) || NEXTEST_SUMMARY.is_match(l))
    {
        return None;
    }
    let mut n = Nextest {
        consumed: vec![false; lines.len()],
        failed: Vec::new(),
        passed: 0,
        summary: None,
    };
    let mut outputs: Vec<(String, Vec<&str>)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(c) = NEXTEST_SUMMARY.captures(line) {
            n.consumed[i] = true;
            n.summary = c.get(1).map(|m| m.as_str());
        } else if let Some(c) = NEXTEST_OUTPUT.captures(line) {
            n.consumed[i] = true;
            let name = format!("{} {}", &c[1], &c[2]);
            let start = i + 1;
            let mut end = start;
            while end < lines.len()
                && !NEXTEST_OUTPUT.is_match(lines[end])
                && !NEXTEST_STATUS.is_match(lines[end])
                && !NEXTEST_SUMMARY.is_match(lines[end])
                && !NEXTEST_RULE.is_match(lines[end])
            {
                n.consumed[end] = true;
                end += 1;
            }
            match outputs.iter_mut().find(|(o, _)| *o == name) {
                Some((_, block)) => block.extend_from_slice(&lines[start..end]),
                None => outputs.push((name, lines[start..end].to_vec())),
            }
            i = end;
            continue;
        } else if NEXTEST_STATUS.is_match(line) {
            n.consumed[i] = true;
            // After the summary, nextest lists the failing tests again.
            if n.summary.is_none() {
                if let Some(c) = NEXTEST_FAIL.captures(line) {
                    let name = format!("{} {}", &c[1], &c[2]);
                    if !n.failed.iter().any(|(f, _)| *f == name) {
                        n.failed.push((name, Vec::new()));
                    }
                } else if NEXTEST_PASS.is_match(line) {
                    n.passed += 1;
                }
            }
        }
        i += 1;
    }
    for (name, block) in &mut n.failed {
        if let Some((_, out)) = outputs.iter().find(|(o, _)| o == name) {
            block.clone_from(out);
        }
    }
    Some(n)
}

impl Nextest<'_> {
    fn apply(self, parsed: &mut Parsed) {
        let found = !self.failed.is_empty();
        for (name, block) in &self.failed {
            // The libtest harness lines nextest captured say only that the test failed.
            let test = name.split_once(' ').map_or("", |(_, t)| t);
            let block: Vec<&str> = block
                .iter()
                .copied()
                .filter(|l| {
                    !RUNNING.is_match(l)
                        && !TEST_LINE.is_match(l)
                        && !RESULT.is_match(l)
                        && l.trim() != "failures:"
                        && l.trim() != test
                })
                .collect();
            parsed.failures.push(test_failure(name, &block));
        }
        if found {
            // cargo's closing words for the failures listed above.
            parsed
                .failures
                .retain(|f| !(f.name == "error" && f.message == "test run failed" && f.file.is_empty()));
            parsed.partial = false;
        }
        parsed.recognised = true;
        let failed = self.failed.len();
        match self.summary {
            Some(summary) => {
                parsed.reported = Some(count_of(summary, "failed"));
                parsed.summary = Some(summary.to_string());
            }
            None if failed + self.passed > 0 => {
                parsed.summary = Some(format!("{} passed, {failed} failed", self.passed));
            }
            None => {}
        }
    }
}

/// Builds the failure for one diagnostic. The first value says whether the block looks
/// like rustc's (an error code or a `-->` location): a bare `error: ...` line could be
/// any program's.
fn diagnostic(code: Option<&str>, severity: &str, message: &str, block: &[&str], after: &[&str]) -> (bool, Failure) {
    let location = block.iter().find_map(|l| ARROW.captures(l));
    let text = block.join("\n");
    let name = code
        .map(str::to_string)
        .or_else(|| lint_name(&CLIPPY_LINT, &text).map(|l| format!("clippy::{l}")))
        .or_else(|| lint_name(&IMPLIED_LINT, &text))
        .or_else(|| lint_name(&LEVEL_LINT, &text))
        .or_else(|| lint_name(&ALLOW_LINT, &text))
        .unwrap_or_else(|| severity.to_string());

    // The location and the lint are in the failure already; the bare gutter says nothing.
    let mut excerpt: Vec<String> = block
        .iter()
        .filter(|l| !ARROW.is_match(l) && !LINT_NOTE.is_match(l) && !EMPTY_GUTTER.is_match(l))
        .map(|l| l.to_string())
        .collect();
    if location.is_none() {
        // cargo's own errors explain themselves under `Caused by:`, after a blank line.
        let mut rest = after.iter().skip_while(|l| l.trim().is_empty());
        if rest.next().is_some_and(|l| l.trim() == "Caused by:") {
            excerpt.extend(rest.take_while(|l| !l.trim().is_empty()).take(6).map(|l| l.to_string()));
        }
    }
    let (file, line) = match &location {
        Some(c) => (c.get(1).map_or("", |m| m.as_str()), number(&c[2])),
        None => ("", None),
    };
    (
        code.is_some() || location.is_some(),
        failure(file, line, &name, message, &clip_lines(excerpt, 10, 2)),
    )
}

/// The lint a diagnostic names, in the spelling `#[allow(...)]` takes.
fn lint_name(pattern: &Regex, text: &str) -> Option<String> {
    pattern.captures(text).map(|c| c[1].replace('-', "_"))
}

/// Builds the failure for one failing test from its captured output.
fn test_failure(name: &str, block: &[&str]) -> Failure {
    let (mut file, mut line, mut message) = (String::new(), None, String::new());
    // Lines whose whole content ends up in the failure's file, line and message.
    let mut consumed: Vec<usize> = Vec::new();
    for (i, l) in block.iter().enumerate() {
        if let Some(c) = OLD_PANIC.captures(l) {
            (file, line, message) = (c[2].to_string(), number(&c[3]), c[1].to_string());
        } else if let Some(c) = PANIC_AT.captures(l) {
            // The message is on the lines after the location.
            let next = (i + 1..block.len()).find(|&j| !block[j].trim().is_empty());
            let text = next.map_or("", |j| block[j].trim());
            (file, line, message) = (c[1].to_string(), number(&c[2]), text.to_string());
            consumed.extend(next);
        } else if let Some(c) = NO_PANIC.captures(l) {
            file = c.get(2).map_or("", |m| m.as_str()).to_string();
            line = c.get(3).and_then(|m| number(m.as_str()));
            message = c[1].to_string();
        } else {
            continue;
        }
        consumed.push(i);
        break;
    }
    if message.is_empty() {
        // A test that returned `Err`, or printed something this parser does not know.
        message = block
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string();
    }
    if let Some(c) = DOCTEST.captures(name) {
        // The panic location inside a doc test is in generated code; the name has the real one.
        (file, line) = (c[1].to_string(), number(&c[2]));
    }
    let excerpt: Vec<String> = block
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            !consumed.contains(i) && !l.trim().is_empty() && !l.starts_with("note: run with `RUST_BACKTRACE")
        })
        .map(|(_, l)| THREAD_ID.replace(l, "$1 panicked").into_owned())
        .collect();
    failure(&file, line, name, &message, &clip_lines(excerpt, 6, 10))
}

/// The first line under `Caused by:`: why cargo says a test binary died.
fn caused_by<'a>(lines: &[&'a str]) -> Option<&'a str> {
    let at = lines.iter().position(|l| l.trim() == "Caused by:")?;
    lines[at + 1..].iter().map(|l| l.trim()).find(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_output;

    // All captured from cargo 1.99; only the project path was shortened.
    const TEST_PASS: &str = include_str!("../../testdata/cargo_test_pass.txt");
    const TEST_FAIL: &str = include_str!("../../testdata/cargo_test_fail.txt");
    const TEST_ABORT: &str = include_str!("../../testdata/cargo_test_abort.txt");
    const BUILD_ERR: &str = include_str!("../../testdata/cargo_build_err.txt");
    const CLIPPY_PASS: &str = include_str!("../../testdata/clippy_pass.txt");
    const CLIPPY_FAIL: &str = include_str!("../../testdata/clippy_fail.txt");
    const NO_MANIFEST: &str = include_str!("../../testdata/cargo_no_manifest.txt");

    fn by_name<'a>(failures: &'a [Failure], name: &str) -> &'a Failure {
        failures
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no failure named {name}"))
    }

    #[test]
    fn passing_runs_are_one_line() {
        assert_eq!(
            filter_output("cargo", TEST_PASS),
            (Vec::new(), "ok: 3 passed; 0 failed; 1 ignored".to_string())
        );
        assert_eq!(filter_output("cargo", CLIPPY_PASS), (Vec::new(), "ok".to_string()));
    }

    #[test]
    fn failing_tests_across_three_binaries() {
        let parsed = parse(TEST_FAIL);
        assert_eq!(parsed.reported, Some(6));
        assert_eq!(parsed.summary.as_deref(), Some("2 passed; 6 failed; 0 ignored"));
        assert_eq!(parsed.failures.len(), 6);

        let assert = by_name(&parsed.failures, "cart::tests::totals_items");
        assert_eq!((assert.file.as_str(), assert.line), ("src/cart.rs", Some(31)));
        assert_eq!(assert.message, "assertion `left == right` failed");
        // What the test printed and the values; the panic line is the file, line and message above.
        assert_eq!(assert.excerpt, "cart has 1 items\n  left: 1.0\n right: 1.01");

        let divide = by_name(&parsed.failures, "pricing::tests::ten_percent");
        assert_eq!((divide.file.as_str(), divide.line), ("src/pricing.rs", Some(2)));
        assert_eq!(divide.message, "attempt to divide by zero");
        assert_eq!(divide.excerpt, "");

        let no_panic = by_name(&parsed.failures, "pricing::tests::must_panic");
        assert_eq!((no_panic.file.as_str(), no_panic.line), ("src/pricing.rs", Some(30)));
        assert_eq!(no_panic.message, "test did not panic as expected");

        let returned_err = by_name(&parsed.failures, "pricing::tests::parses");
        assert_eq!(returned_err.file, "");
        assert_eq!(
            returned_err.message,
            "Error: \"bad percent \\\"ten\\\": invalid digit found in string\""
        );

        let integration = by_name(&parsed.failures, "order_total_matches_cart");
        assert_eq!(
            (integration.file.as_str(), integration.line),
            ("tests/orders.rs", Some(7))
        );
        assert_eq!(integration.message, "total was 4");

        let doc = by_name(&parsed.failures, "src/lib.rs - add_one (line 6)");
        assert_eq!((doc.file.as_str(), doc.line), ("src/lib.rs", Some(6)));
        assert_eq!(doc.message, "assertion `left == right` failed");
        assert_eq!(
            doc.excerpt,
            "Test executable failed (exit status: 101).\nstderr:\n  left: 2\n right: 3"
        );
    }

    #[test]
    fn compile_errors_are_failures_and_warnings_are_kept_apart() {
        let parsed = parse(BUILD_ERR);
        let got: Vec<(&str, Option<u32>, &str, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.name.as_str(), f.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("src/pricing.rs", Some(5), "E0308", "mismatched types"),
                (
                    "src/pricing.rs",
                    Some(10),
                    "E0425",
                    "cannot find function `prices` in this scope"
                ),
            ]
        );
        assert_eq!(
            parsed.failures[0].excerpt,
            "5 |     let rate: i64 = \"ten\";\n  |               ---   ^^^^^ expected `i64`, found `&str`\n  |               |\n  |               expected due to this"
        );
        assert_eq!(parsed.warnings.len(), 1);
        assert_eq!(parsed.warnings[0].name, "unused_imports");
        assert_eq!(parsed.warnings[0].line, Some(1));
        assert_eq!(parsed.reported, None);

        let (failures, text) = filter_output("cargo", BUILD_ERR);
        assert_eq!(failures.len(), 2);
        assert!(
            !text.contains("unused import"),
            "warnings stay out when there are errors"
        );
        assert!(!text.contains("could not compile"));
    }

    #[test]
    fn clippy_lints_are_named() {
        let parsed = parse(CLIPPY_FAIL);
        let mut got: Vec<(Option<u32>, &str)> = parsed.failures.iter().map(|f| (f.line, f.name.as_str())).collect();
        got.sort();
        assert_eq!(
            got,
            [
                (Some(2), "clippy::needless_return"),
                (Some(6), "clippy::needless_bool"),
                (Some(7), "clippy::needless_return"),
                (Some(9), "clippy::needless_return"),
                (Some(13), "clippy::ptr_arg"),
                (Some(14), "unused_variables"),
            ]
        );
        assert!(parsed.failures.iter().all(|f| f.file == "src/pricing.rs"));
        let ptr_arg = by_name(&parsed.failures, "clippy::ptr_arg");
        assert!(ptr_arg.message.starts_with("writing `&Vec` instead of `&[_]`"));
        assert!(
            ptr_arg.excerpt.contains("13 + pub fn first(items: &[i64]) -> i64 {"),
            "the suggested fix stays"
        );
        assert!(!ptr_arg.excerpt.contains("rust-lang.github.io") && !ptr_arg.excerpt.contains("-->"));
    }

    #[test]
    fn a_test_binary_that_dies_names_the_test_that_was_running() {
        let (failures, text) = filter_output("cargo", TEST_ABORT);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "pricing::tests::zz_aborts");
        assert!(failures[0].message.contains("signal: 6, SIGABRT"));
        assert!(text.starts_with("(no file)\n  pricing::tests::zz_aborts: process didn't exit successfully"));
    }

    #[test]
    fn cargo_errors_without_a_location() {
        let (failures, text) = filter_output("cargo", NO_MANIFEST);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "error");
        assert_eq!(
            text,
            "(no file)\n  error: could not find `Cargo.toml` in `/home/dev/empty` or any parent directory\n"
        );
        // The same line from an unknown tool is not claimed as a cargo error.
        assert!(!parse(NO_MANIFEST).recognised);
    }

    #[test]
    fn caused_by_explains_a_cargo_error() {
        let raw = "error: failed to parse manifest at `/home/dev/shop/Cargo.toml`\n\nCaused by:\n  \
                   TOML parse error at line 5, column 1\n  expected `=`\n";
        let parsed = parse(raw);
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(
            parsed.failures[0].excerpt,
            "TOML parse error at line 5, column 1\nexpected `=`"
        );
    }

    #[test]
    fn a_second_panic_keeps_its_line_without_the_thread_id() {
        let block = [
            "thread 't::a' (41) panicked at src/a.rs:3:5:",
            "first",
            "thread 'worker' (42) panicked at src/b.rs:9:1:",
            "second",
        ];
        let f = test_failure("t::a", &block);
        assert_eq!(
            (f.file.as_str(), f.line, f.message.as_str()),
            ("src/a.rs", Some(3), "first")
        );
        assert_eq!(f.excerpt, "thread 'worker' panicked at src/b.rs:9:1:\nsecond");
    }

    #[test]
    fn old_panic_format_and_a_cut_run() {
        // No `failures:` list and no result line: the run was killed mid-way.
        let raw = "running 2 tests\ntest a::one ... FAILED\ntest a::two ... ok\n\nfailures:\n\n\
                   ---- a::one stdout ----\nthread 'a::one' panicked at 'assertion failed: x == 1', src/a.rs:10:5\n";
        let parsed = parse(raw);
        let one = by_name(&parsed.failures, "a::one");
        assert_eq!(
            (one.file.as_str(), one.line, one.message.as_str()),
            ("src/a.rs", Some(10), "assertion failed: x == 1")
        );
        assert!(parsed.failures.iter().any(|f| f.name == "(test binary)"));
    }
}
