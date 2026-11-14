//! The text that goes back to the model.
//!
//! ```text
//! [lint] ruff: FAIL 2 failures (exit 1)
//! shop/orders.py
//!   1: F401: `os` imported but unused
//!   22: E722: Do not use bare `except`
//! summary: Found 2 errors.
//! [types] mypy: PASS (no issues found in 14 source files)
//! diff of the files named above:
//! --- a/shop/orders.py
//! +++ b/shop/orders.py
//! @@ -1,3 +1,4 @@
//! ...
//! raw 48211 bytes, filtered 1320 bytes
//! ```
//!
//! No timings and no absolute paths of the machine: the same failures give the same
//! bytes on every run.

use crate::{CheckResult, Report};

/// Renders a report: one header line per check in the order they ran, each failed
/// check followed by its failures grouped by file, then the diff hunks, then one line
/// with the raw and filtered sizes.
pub fn render(report: &Report) -> String {
    let mut out = body(report);
    out.push_str(&size_line(report.raw_bytes, report.filtered_bytes));
    out
}

/// Length of the text [`render`] returns once `filtered_bytes` is set to that length.
/// The last line states the size of the whole text, itself included, so the number is
/// found by repeating until it stops changing (it grows by a digit at most).
pub(crate) fn rendered_len(report: &Report) -> usize {
    let body = body(report).len();
    let mut total = body;
    loop {
        let next = body + size_line(report.raw_bytes, total).len();
        if next == total {
            return total;
        }
        total = next;
    }
}

fn size_line(raw: usize, filtered: usize) -> String {
    format!("raw {raw} bytes, filtered {filtered} bytes\n")
}

fn body(report: &Report) -> String {
    let mut out = String::new();
    if report.results.is_empty() {
        out.push_str("no checks ran\n");
    }
    for result in &report.results {
        out.push_str(&header(result));
        out.push('\n');
        if !result.passed {
            push_block(&mut out, &result.filtered);
        }
    }
    if !report.diff_hunks.trim().is_empty() {
        out.push_str("diff of the files named above:\n");
        push_block(&mut out, &report.diff_hunks);
    }
    out
}

/// Appends `text`, making sure the next thing starts on its own line.
fn push_block(out: &mut String, text: &str) {
    out.push_str(text);
    if !text.is_empty() && !text.ends_with('\n') {
        out.push('\n');
    }
}

/// `[tests] pytest: FAIL 4 failures (exit 1)` or `[types] mypy: PASS (summary)`.
fn header(result: &CheckResult) -> String {
    let mut line = format!("[{}] {}: ", result.kind.as_str(), result.name);
    if result.passed {
        line.push_str("PASS");
        // A passing check is filtered to `ok` or `ok: <the tool's summary>`.
        let first = result.filtered.lines().next().unwrap_or("").trim();
        let summary = first
            .strip_prefix("ok: ")
            .unwrap_or(if first == "ok" { "" } else { first });
        if !summary.is_empty() {
            line.push_str(&format!(" ({summary})"));
        }
        return line;
    }
    line.push_str("FAIL");
    if result.timed_out {
        line.push_str(" timed out");
    }
    match result.failures.len() {
        0 => {}
        1 => line.push_str(" 1 failure"),
        n => line.push_str(&format!(" {n} failures")),
    }
    match result.exit_code {
        Some(code) => line.push_str(&format!(" (exit {code})")),
        None if !result.timed_out => line.push_str(" (killed)"),
        None => {}
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckKind, Failure};

    fn result(name: &str, kind: CheckKind, exit_code: Option<i32>, failures: usize, filtered: &str) -> CheckResult {
        CheckResult {
            name: name.into(),
            kind,
            command: name.into(),
            exit_code,
            passed: exit_code == Some(0),
            timed_out: false,
            duration_ms: 1234,
            raw_bytes: 1_000,
            raw_lines: 20,
            failures: (0..failures)
                .map(|i| Failure {
                    file: "a.py".into(),
                    line: Some(i as u32 + 1),
                    name: format!("E{i}"),
                    message: "m".into(),
                    excerpt: String::new(),
                })
                .collect(),
            filtered: filtered.into(),
        }
    }

    fn report(results: Vec<CheckResult>, diff_hunks: &str) -> Report {
        let mut report = Report {
            passed: results.iter().all(|r| r.passed),
            raw_bytes: results.iter().map(|r| r.raw_bytes).sum(),
            results,
            diff_hunks: diff_hunks.into(),
            filtered_bytes: 0,
        };
        report.filtered_bytes = rendered_len(&report);
        report
    }

    #[test]
    fn full_report() {
        let mut timed_out = result("slow", CheckKind::Tests, None, 0, "timed out after 5s\nstarted\n");
        timed_out.timed_out = true;
        let report = report(
            vec![
                result(
                    "ruff",
                    CheckKind::Lint,
                    Some(1),
                    2,
                    "a.py\n  1: E0: m\n  2: E1: m\nsummary: Found 2 errors.\n",
                ),
                result(
                    "mypy",
                    CheckKind::Types,
                    Some(0),
                    0,
                    "ok: no issues found in 14 source files",
                ),
                result("tsc", CheckKind::Types, Some(0), 0, "ok"),
                result("pytest", CheckKind::Tests, Some(1), 1, "a.py\n  1: E0: m"),
                result("make", CheckKind::Tests, Some(2), 0, "make: *** [all] Error 2\n"),
                result("crash", CheckKind::Tests, None, 0, "(no output)\n"),
                timed_out,
            ],
            "--- a/a.py\n+++ b/a.py\n@@ -1,1 +1,1 @@\n-x\n+y\n",
        );
        let text = render(&report);
        assert_eq!(
            text,
            "[lint] ruff: FAIL 2 failures (exit 1)\n\
             a.py\n\
             \x20 1: E0: m\n\
             \x20 2: E1: m\n\
             summary: Found 2 errors.\n\
             [types] mypy: PASS (no issues found in 14 source files)\n\
             [types] tsc: PASS\n\
             [tests] pytest: FAIL 1 failure (exit 1)\n\
             a.py\n\
             \x20 1: E0: m\n\
             [tests] make: FAIL (exit 2)\n\
             make: *** [all] Error 2\n\
             [tests] crash: FAIL (killed)\n\
             (no output)\n\
             [tests] slow: FAIL timed out\n\
             timed out after 5s\n\
             started\n\
             diff of the files named above:\n\
             --- a/a.py\n\
             +++ b/a.py\n\
             @@ -1,1 +1,1 @@\n\
             -x\n\
             +y\n\
             raw 7000 bytes, filtered 479 bytes\n"
        );
        assert_eq!(text.len(), 479);
        assert!(!report.passed);
        // Rendering is a pure function of the report.
        assert_eq!(render(&report), text);
    }

    #[test]
    fn the_size_line_counts_itself() {
        // Sizes around a change in the number of digits are where a fixed point could slip.
        for padding in 0..120 {
            let filtered = "x".repeat(padding);
            let report = report(vec![result("c", CheckKind::Lint, Some(1), 0, &filtered)], "");
            assert_eq!(render(&report).len(), report.filtered_bytes, "padding {padding}");
        }
        let big = report(vec![result("c", CheckKind::Lint, Some(1), 0, &"y".repeat(99_950))], "");
        assert_eq!(render(&big).len(), big.filtered_bytes);
    }

    #[test]
    fn all_passing_and_empty_reports() {
        let passing = report(vec![result("clippy", CheckKind::Lint, Some(0), 0, "ok")], "");
        assert_eq!(
            render(&passing),
            "[lint] clippy: PASS\nraw 1000 bytes, filtered 54 bytes\n"
        );
        assert!(passing.passed);
        let empty = report(Vec::new(), "");
        assert_eq!(render(&empty), "no checks ran\nraw 0 bytes, filtered 45 bytes\n");
    }
}
