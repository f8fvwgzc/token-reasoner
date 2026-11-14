//! ruff and flake8: one line per violation, `path:line:col: CODE message`.
//!
//! ruff has three shapes: `concise` (that line alone), the older `full` (that line
//! followed by a source snippet) and the newer `full`, where the code comes first and
//! the location follows on a ` --> path:line:col` line.

use super::{number, pattern, Parsed};
use crate::format::{clip_lines, failure};

pattern!(
    CONCISE,
    r"^([^\s:][^:]*):(\d+):(\d+): ([A-Z]+[0-9]+|SyntaxError|invalid-syntax)(?: \[\*\])?:? (.*)$"
);
pattern!(
    HEAD,
    r"^([A-Z]+[0-9]+|invalid-syntax|[a-z]+(?:-[a-z]+)+)(?: \[\*\])?:? (.+)$"
);
pattern!(ARROW, r"^\s*--> (.+?):(\d+):(\d+)$");
pattern!(SNIPPET, r"^\s*(?:\d+\s*)?\||^\s+= |^(?:help|info|note): |^\s*::: ");
pattern!(FOUND, r"^Found (\d+) errors?(?: \((\d+) fixed, (\d+) remaining\))?\.$");
// A snippet line with nothing but the gutter bar.
pattern!(EMPTY_GUTTER, r"^\s*\|\s*$");

pub(crate) fn parse(text: &str) -> Parsed {
    let lines: Vec<&str> = text.lines().collect();
    let mut parsed = Parsed::default();
    let mut found: Option<(String, Option<usize>)> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if let Some(c) = FOUND.captures(line) {
            let count = c.get(3).or(c.get(1)).and_then(|m| m.as_str().parse().ok());
            found = Some((line.to_string(), count));
        } else if line == "All checks passed!" {
            parsed.recognised = true;
            parsed.summary = Some(line.to_string());
        } else if let Some(c) = CONCISE.captures(line) {
            let excerpt = snippet(&lines, &mut i);
            parsed
                .failures
                .push(failure(&c[1], number(&c[2]), &c[4], &c[5], &excerpt));
        } else if let (Some(head), Some(at)) = (HEAD.captures(line), lines.get(i).and_then(|l| ARROW.captures(l))) {
            i += 1;
            let excerpt = snippet(&lines, &mut i);
            parsed
                .failures
                .push(failure(&at[1], number(&at[2]), &head[1], &head[2], &excerpt));
        }
    }
    // tsc closes with the same `Found 1 error.`: the line is ruff's only next to violations.
    if let Some((summary, reported)) = found.filter(|_| !parsed.failures.is_empty()) {
        parsed.summary = Some(summary);
        parsed.reported = reported;
    }
    parsed.recognised |= !parsed.failures.is_empty();
    parsed
}

/// The source snippet under a violation, when the format has one.
fn snippet(lines: &[&str], i: &mut usize) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(line) = lines.get(*i).filter(|l| SNIPPET.is_match(l)) {
        if !EMPTY_GUTTER.is_match(line) {
            out.push(line.to_string());
        }
        *i += 1;
    }
    clip_lines(out, 6, 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_output;

    const PASS: &str = include_str!("../../testdata/ruff_pass.txt");
    const CONCISE_RUN: &str = include_str!("../../testdata/ruff_fail_concise.txt");
    const FULL: &str = include_str!("../../testdata/ruff_fail_full.txt");
    const NEW: &str = include_str!("../../testdata/ruff_fail_new.txt");
    const CRASH: &str = include_str!("../../testdata/ruff_crash.txt");
    const FLAKE8: &str = include_str!("../../testdata/flake8_fail.txt");
    const FLAKE8_CRASH: &str = include_str!("../../testdata/flake8_crash.txt");

    fn codes(text: &str) -> Vec<(String, Option<u32>, String)> {
        parse(text)
            .failures
            .into_iter()
            .map(|f| (f.file, f.line, f.name))
            .collect()
    }

    fn code(file: &str, line: u32, name: &str) -> (String, Option<u32>, String) {
        (file.to_string(), Some(line), name.to_string())
    }

    #[test]
    fn passing_run_is_one_line() {
        assert_eq!(
            filter_output("ruff", PASS),
            (Vec::new(), "ok: All checks passed!".to_string())
        );
        // flake8 prints nothing when there is nothing to report.
        assert_eq!(filter_output("flake8", ""), (Vec::new(), "ok".to_string()));
    }

    #[test]
    fn concise_format() {
        let parsed = parse(CONCISE_RUN);
        assert_eq!(parsed.reported, Some(5));
        assert_eq!(parsed.failures.len(), 5);
        assert_eq!(
            codes(CONCISE_RUN),
            [
                code("shop/orders.py", 1, "F401"),
                code("shop/orders.py", 22, "E722"),
                code("shop/pricing.py", 3, "E501"),
                code("shop/pricing.py", 12, "B008"),
                code("tests/test_cart.py", 1, "I001"),
            ]
        );
        assert_eq!(parsed.failures[0].message, "`os` imported but unused");
        assert!(parsed.failures.iter().all(|f| f.excerpt.is_empty()));
    }

    #[test]
    fn full_format_keeps_the_snippet() {
        let parsed = parse(FULL);
        assert_eq!(parsed.reported, Some(3));
        assert_eq!(
            codes(FULL),
            [
                code("shop/orders.py", 1, "F401"),
                code("shop/orders.py", 22, "E722"),
                code("shop/pricing.py", 7, "SyntaxError"),
            ]
        );
        assert_eq!(
            parsed.failures[0].excerpt,
            "1 | import os\n  |        ^^ F401\n2 | import json\n  = help: Remove unused import: `os`"
        );
        assert_eq!(
            parsed.failures[2].message,
            "Expected an indented block after function definition"
        );
    }

    #[test]
    fn code_first_format() {
        let parsed = parse(NEW);
        assert_eq!(parsed.reported, Some(3));
        assert_eq!(
            codes(NEW),
            [
                code("shop/orders.py", 1, "F401"),
                code("shop/orders.py", 22, "E722"),
                code("shop/pricing.py", 7, "invalid-syntax"),
            ]
        );
        assert_eq!(parsed.failures[1].message, "Do not use bare `except`");
        assert!(parsed.failures[0].excerpt.ends_with("help: Remove unused import: `os`"));
        assert!(parsed.failures[1].excerpt.contains("22 |     except:"));
    }

    #[test]
    fn flake8_lines() {
        let (failures, text) = filter_output("flake8", FLAKE8);
        assert_eq!(failures.len(), 4);
        assert_eq!(failures[3].name, "E999");
        assert_eq!(failures[3].message, "SyntaxError: '(' was never closed");
        assert_eq!(
            text,
            "shop/orders.py\n  1: F401: 'os' imported but unused\n  22: E722: do not use bare 'except'\n\
             shop/pricing.py\n  3: E501: line too long (104 > 79 characters)\n\
             tests/test_cart.py\n  9: E999: SyntaxError: '(' was never closed\n"
        );
    }

    #[test]
    fn fixed_and_remaining_counts() {
        let parsed = parse("a.py:1:1: E722 Do not use bare `except`\nFound 3 errors (2 fixed, 1 remaining).\n");
        assert_eq!(parsed.reported, Some(1));
    }

    #[test]
    fn a_closing_line_without_violations_is_not_claimed() {
        let parsed = parse("error TS5058: The specified path does not exist: 'nope.json'.\n\nFound 1 error.\n");
        assert!(!parsed.recognised && parsed.reported.is_none());
    }

    #[test]
    fn a_crash_is_shown_whole() {
        for (tool, raw) in [("ruff", CRASH), ("flake8", FLAKE8_CRASH)] {
            assert!(!parse(raw).recognised);
            let (failures, text) = filter_output(tool, raw);
            assert!(failures.is_empty());
            assert_eq!(text, raw);
        }
    }
}
